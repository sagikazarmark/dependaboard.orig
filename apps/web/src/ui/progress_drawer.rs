//! The drawer that follows a bulk action, one row per target, and says how
//! long the batch has stood still and whether the server is answering. It
//! never says the batch is lost: a target may sit for hours inside GitHub's
//! retry and rate-limit budgets, and the batch is durable in Restate for all
//! of them. For a batch known by id alone it says which way of finding it the
//! follow is on until there is progress to show. Once the server has refused
//! the credentials it says the follow has stopped, and that the reload which
//! signs in again picks it up.

use dependaboard_core::TargetProgressState;
use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::ui::batch::{Followed, Listing};
use crate::ui::format::{ago, pull_requests, relative_time, verdict_tally};
use crate::ui::retry::can_retry;
use crate::ui::side_panel::SidePanel;

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
        let title = match followed.listing.action() {
            Some(action) => format!("{action} progress"),
            None => "Batch progress".to_owned(),
        };
        return rsx! {
            SidePanel {
                class: "progress-drawer",
                eyebrow: "Batch {followed.batch_id}",
                title: rsx! { "{title}" },
                onclose,
                if !followed.signed_out() {
                    p { class: "progress-note", "{finding_note(&followed, now)}" }
                }
                if let Some(note) = trouble_note(&followed) {
                    p { class: "progress-note progress-trouble", "{note}" }
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
                span { "{verdict_tally(progress.succeeded, progress.rejected, progress.failed)}" }
                progress { class: "progress progress-primary", max: "100", value: "{percentage}" }
            }
            if let Some(note) = waiting_note(&followed, now) {
                p { class: "progress-note", "{note}" }
            }
            if let Some(note) = trouble_note(&followed) {
                p { class: "progress-note progress-trouble", "{note}" }
            }
            div { class: "progress-list",
                for item in &progress.targets {
                    TargetRow {
                        owner: item.target.owner.clone(),
                        repo: item.target.repo.clone(),
                        number: item.target.number,
                        title: item.target.title.clone(),
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

/// What the drawer says of a batch followed by id alone while there is no
/// progress to show: which of the ways of finding a batch the follow is on.
/// The projection is asked first, so until it has answered the batch is being
/// looked up; a listing as running is passed on — what was asked, over how
/// many, by whom, since when — with Restate asked for the rest; an id the
/// projection has never heard of is said to be, with why that need not be the
/// end of it, while Restate is given its say; and a projection that could not
/// be read is said to be, with Restate asked as it always was. The fault
/// itself is the trouble note's to pass on, for as long as it lasts.
fn finding_note(followed: &Followed, now: u64) -> String {
    match &followed.listing {
        Listing::Unasked => "Looking the batch up...".to_owned(),
        Listing::Unreadable => {
            "The projection could not be read. Asking Restate where the batch stands...".to_owned()
        }
        Listing::Unlisted => "The projection has no batch by this id, running or finished; one \
                              just queued may not be listed yet. Asking Restate where it stands..."
            .to_owned(),
        Listing::Running(listing) => format!(
            "Listed as running: {} over {}, by {}, started {}. Asking Restate where it stands...",
            listing.action,
            pull_requests(listing.target_count),
            listing.requested_by,
            ago(now, listing.started_at)
        ),
    }
}

/// What the drawer says about a batch that is not moving, if anything: nothing
/// while [`Followed::stands_still`] says it does not. Past that, for how long
/// and what the dashboard can tell of why: while Restate has not spoken for a
/// batch the dashboard submitted, the wait is on Restate to start it. Once it
/// has, the dashboard cannot tell a call being retried inside its budgets from
/// a service that has died — the workflow publishes progress only as verdicts
/// land, so the two look the same from here — so it says only for how long,
/// and does not name GitHub as the cause.
fn waiting_note(followed: &Followed, now: u64) -> Option<String> {
    if !followed.stands_still(now) {
        return None;
    }
    let standing = relative_time(now, followed.since);
    Some(if followed.heard {
        format!(
            "No progress for {standing}. Whether a call is being retried or the service is down, \
             the dashboard cannot tell; one call may take hours inside its budgets, and the batch \
             is durable in Restate for all of them."
        )
    } else {
        format!(
            "Waiting on Restate to start the batch, {standing} after it took it: its service may be \
             down or deploying, and the batch starts when it is back."
        )
    })
}

/// What the drawer says about the polls, if anything: nothing while they are
/// answered. While they are not, the fault — and, once Restate has been heard
/// on the batch, that it carries on and the dashboard keeps asking after it,
/// since a poll that fails says nothing about the batch; before Restate has
/// been heard, the line above already says the dashboard is asking. Once the
/// server has refused the credentials the dashboard has stopped asking, since
/// asking again would be refused again and prompt for them each time, and the
/// note says so and what brings the follow back: the reload that signs in
/// again, which picks the batch up from the URL.
fn trouble_note(followed: &Followed) -> Option<String> {
    let trouble = followed.trouble.as_ref()?;
    Some(match (followed.signed_out(), followed.progress.is_some()) {
        (true, true) => format!(
            "{trouble}. The batch carries on in Restate; the dashboard has stopped asking after it, \
             and picks it up again once the page is reloaded."
        ),
        (true, false) => format!(
            "{trouble}. The dashboard has stopped asking where the batch stands, and asks again once \
             the page is reloaded."
        ),
        (false, true) => {
            format!(
                "{trouble}. The batch carries on in Restate; the dashboard keeps asking after it."
            )
        }
        (false, false) => trouble.to_string(),
    })
}

/// One target of a batch: its state as a dot, the pull request as a link to
/// GitHub when its URL is known (a target recorded before URLs travelled with
/// it has none), its title beside the reference — the reference says where,
/// the title says which dependency and versions — and the state's detail.
/// Shared by the drawer that follows a running batch and the list of the
/// batches that have run.
#[component]
pub(crate) fn TargetRow(
    owner: String,
    repo: String,
    number: u64,
    title: String,
    html_url: String,
    state: TargetProgressState,
) -> Element {
    rsx! {
        div { class: "progress-row",
            span { class: "progress-state {progress_class(&state)}" }
            div {
                div { class: "progress-target",
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
                    if !title.is_empty() {
                        span { class: "progress-title", title: "{title}", "{title}" }
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
    use dependaboard_core::{
        ActionOutcome, BatchProgress, BulkActionKind, RejectReason, RunningBatch, UserId,
    };

    use super::*;
    use crate::ui::Fault;
    use crate::ui::batch::{Listing, WAITING_NOTICE_AFTER};
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
    /// lost, and the drawer does not say so; nor does it say the wait is on
    /// GitHub, which it cannot know — progress is published only as verdicts
    /// land, so a call being retried and a service that has died look the
    /// same from here. It says for how long the batch has stood, that it
    /// cannot tell why, and that the batch carries on.
    #[test]
    fn a_batch_standing_still_says_for_how_long_and_neither_that_it_is_lost_nor_whose_fault() {
        let html = render_followed(followed(half_done_merge()), FIXTURE_NOW + 47 * 60, false);

        assert!(
            html.contains(
                r#"<p class="progress-note">No progress for 47m. Whether a call is being retried or the service is down, the dashboard cannot tell; one call may take hours inside its budgets, and the batch is durable in Restate for all of them.</p>"#
            ),
            "{html}"
        );
        assert!(!html.to_lowercase().contains("lost"), "{html}");
        assert!(!html.contains("GitHub"), "{html}");
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
        assert!(!html.contains("No progress for"), "{html}");
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
            trouble: Some(Fault::Refused("Restate is unavailable".to_owned())),
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

    /// The server refused the credentials the poll carried, and the follow
    /// stopped there rather than prompt for them every second. The drawer
    /// says so over the progress last heard — the batch carries on without
    /// being asked after — and that the reload that signs in again is what
    /// picks it up; it does not claim to be still asking.
    #[test]
    fn a_refusal_of_the_credentials_says_the_follow_has_stopped_and_what_brings_it_back() {
        let refused = Followed {
            trouble: Some(Fault::SignedOut),
            ..followed(half_done_merge())
        };

        let html = render_followed(refused, FIXTURE_NOW, false);

        assert!(
            html.contains(
                r#"<p class="progress-note progress-trouble">You are no longer signed in — reload to sign in again. The batch carries on in Restate; the dashboard has stopped asking after it, and picks it up again once the page is reloaded.</p>"#
            ),
            "{html}"
        );
        assert!(!html.contains("keeps asking"), "{html}");
        assert!(html.contains("1/2"), "{html}");
    }

    /// A batch known by id alone whose polls were refused before Restate
    /// answered any: the dashboard is no longer asking where it stands, and
    /// does not say it is; there is nothing to say the batch carries on in
    /// Restate either, since Restate was never heard on it.
    #[test]
    fn a_batch_followed_by_id_alone_refused_the_credentials_no_longer_says_it_is_asking() {
        let refused = Followed {
            trouble: Some(Fault::SignedOut),
            ..Followed::attaching("batch-1", FIXTURE_NOW)
        };

        let html = render_followed(refused, FIXTURE_NOW, false);

        assert!(!html.contains("Asking Restate"), "{html}");
        assert!(
            html.contains(
                r#"<p class="progress-note progress-trouble">You are no longer signed in — reload to sign in again. The dashboard has stopped asking where the batch stands, and asks again once the page is reloaded.</p>"#
            ),
            "{html}"
        );
        assert!(!html.contains("carries on in Restate"), "{html}");
        assert!(html.contains("Batch batch-1"), "{html}");
    }

    /// After a reload the dashboard has the id alone; until the projection has
    /// answered the drawer names the batch and says it is looking it up — not
    /// that it is asking Restate, which is the second word, not the first.
    #[test]
    fn a_batch_followed_by_id_alone_says_it_is_being_looked_up() {
        let attaching = Followed::attaching("batch-1", FIXTURE_NOW);

        let html = render_followed(attaching, FIXTURE_NOW, false);

        assert!(html.contains("Batch batch-1"), "{html}");
        assert!(
            html.contains(r#"<p class="progress-note">Looking the batch up...</p>"#),
            "{html}"
        );
        assert!(!html.contains("Asking Restate"), "{html}");
        assert!(!html.contains("progress-summary"), "{html}");
        assert!(!html.contains(RETRY_BUTTON), "{html}");
    }

    /// The projection lists the batch as running: the drawer says so, with
    /// what the listing knows of it — what was asked, over how many, by whom,
    /// since when — and that Restate is being asked for the rest. The title
    /// names the action, as it does once the progress is in.
    #[test]
    fn a_batch_the_projection_lists_as_running_is_described_from_the_listing_while_restate_is_asked()
     {
        let listed = Followed {
            listing: Listing::Running(RunningBatch {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                requested_by: UserId::new("alice"),
                started_at: FIXTURE_NOW - 3 * 60,
                target_count: 12,
            }),
            ..Followed::attaching("batch-1", FIXTURE_NOW)
        };

        let html = render_followed(listed, FIXTURE_NOW, false);

        assert!(html.contains("merge progress"), "{html}");
        assert!(
            html.contains(
                r#"<p class="progress-note">Listed as running: merge over 12 pull requests, by alice, started 3m ago. Asking Restate where it stands...</p>"#
            ),
            "{html}"
        );
        assert!(!html.contains("progress-summary"), "{html}");
    }

    /// The projection could not be read, and the store's fault has since
    /// cleared from the polls: the drawer still says the batch is on Restate's
    /// word alone, rather than that it is being looked up, which it no longer
    /// is.
    #[test]
    fn a_batch_whose_projection_could_not_be_read_says_so_while_restate_is_asked() {
        let unreadable = Followed {
            listing: Listing::Unreadable,
            ..Followed::attaching("batch-1", FIXTURE_NOW)
        };

        let html = render_followed(unreadable, FIXTURE_NOW, false);

        assert!(
            html.contains(
                r#"<p class="progress-note">The projection could not be read. Asking Restate where the batch stands...</p>"#
            ),
            "{html}"
        );
        assert!(!html.contains("Looking the batch up"), "{html}");
    }

    /// The projection has never heard of the id: the drawer says so, and why
    /// that need not be the end of it, and that Restate is being asked.
    #[test]
    fn a_batch_the_projection_has_never_heard_of_says_so_while_restate_is_asked() {
        let unlisted = Followed {
            listing: Listing::Unlisted,
            ..Followed::attaching("batch-1", FIXTURE_NOW)
        };

        let html = render_followed(unlisted, FIXTURE_NOW, false);

        assert!(html.contains("Batch batch-1"), "{html}");
        assert!(
            html.contains(
                r#"<p class="progress-note">The projection has no batch by this id, running or finished; one just queued may not be listed yet. Asking Restate where it stands...</p>"#
            ),
            "{html}"
        );
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
                    title: "build(deps): bump serde from 1.0.1 to 1.0.2",
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
                    title: "build(deps): bump serde from 1.0.1 to 1.0.2",
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

    /// The reference says where the pull request is; its title, beside it,
    /// says what — the dependency and the versions — whether or not the
    /// reference is linked, and whole on hover where the row is too narrow
    /// for it. A target with no title on record has nothing beside it.
    #[test]
    fn a_target_row_shows_the_pull_requests_title_beside_its_reference() {
        fn Linked() -> Element {
            rsx! {
                TargetRow {
                    owner: "acme",
                    repo: "api",
                    number: 9,
                    title: "build(deps): bump serde from 1.0.1 to 1.0.2",
                    html_url: "https://github.example/acme/api/pull/9",
                    state: TargetProgressState::Queued,
                }
            }
        }
        let linked = render(Linked);
        assert!(
            linked.contains(
                r#"</a><span class="progress-title" title="build(deps): bump serde from 1.0.1 to 1.0.2">build(deps): bump serde from 1.0.1 to 1.0.2</span>"#
            ),
            "{linked}"
        );

        fn Unlinked() -> Element {
            rsx! {
                TargetRow {
                    owner: "acme",
                    repo: "api",
                    number: 9,
                    title: "build(deps): bump serde from 1.0.1 to 1.0.2",
                    html_url: "",
                    state: TargetProgressState::Queued,
                }
            }
        }
        let unlinked = render(Unlinked);
        assert!(
            unlinked.contains(
                r#"<strong>acme/api#9</strong><span class="progress-title" title="build(deps): bump serde from 1.0.1 to 1.0.2">build(deps): bump serde from 1.0.1 to 1.0.2</span>"#
            ),
            "{unlinked}"
        );

        fn Untitled() -> Element {
            rsx! {
                TargetRow {
                    owner: "acme",
                    repo: "api",
                    number: 9,
                    title: "",
                    html_url: "",
                    state: TargetProgressState::Queued,
                }
            }
        }
        let untitled = render(Untitled);
        assert!(!untitled.contains("progress-title"), "{untitled}");
    }

    /// The drawer that follows a batch and the list of the batches that have
    /// run share the row, so a target's title is beside its reference here
    /// too, from the target the batch was submitted with.
    #[test]
    fn a_followed_batchs_rows_carry_each_targets_title() {
        let html = render_drawer(half_done_merge(), false);

        assert!(
            html.contains(">build(deps): bump serde from 1.0.1 to 1.0.2</span>"),
            "{html}"
        );
    }
}
