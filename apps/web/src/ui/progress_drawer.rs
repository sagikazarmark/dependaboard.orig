//! The drawer that follows a bulk action, one row per target, and says how
//! long the batch has stood still and whether the server is answering. It
//! never says the batch is lost: a target may sit for hours inside GitHub's
//! retry and rate-limit budgets, and the batch is durable in Restate for all
//! of them. For a batch known by id alone it says which way of finding it the
//! follow is on until there is progress to show. Once the server has refused
//! the credentials it says the follow has stopped, and that the reload which
//! signs in again picks it up.

use dependaboard_core::{TargetProgressState, short_sha};
use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::ui::batch::{Followed, Listing};
use crate::ui::format::{ago, commit_url, pull_requests, relative_time, verdict_tally};
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
/// the title says which dependency and versions — and the state's detail, with
/// the commit a merge made beside it once it is known. Shared by the drawer
/// that follows a running batch and the list of the batches that have run.
#[component]
pub(crate) fn TargetRow(
    owner: String,
    repo: String,
    number: u64,
    title: String,
    html_url: String,
    state: TargetProgressState,
) -> Element {
    let merge_sha = state.merge_sha().map(str::to_owned);
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
                            href: html_url.clone(),
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
                small {
                    "{progress_detail(&state)}"
                    if let Some(sha) = merge_sha {
                        " · "
                        CommitRef { sha, pull_request_url: html_url }
                    }
                }
            }
        }
    }
}

/// The commit `sha` in passing — its first seven characters, the whole on
/// hover — as a link to the commit on GitHub when the pull request it was
/// made from has a page to find it under, and as plain text when it has none.
#[component]
fn CommitRef(sha: String, pull_request_url: String) -> Element {
    let short = short_sha(&sha).to_owned();
    match commit_url(&pull_request_url, &sha) {
        Some(href) => rsx! {
            a {
                class: "commit-link",
                href,
                target: "_blank",
                rel: "noreferrer",
                title: "{sha}",
                code { "{short}" }
            }
        },
        None => rsx! {
            code { class: "commit-ref", title: "{sha}", "{short}" }
        },
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

/// The state in words: the words GitHub had for what it did, or the reason it
/// did not. A merge's commit is not among them; [`TargetRow`] sets it beside.
fn progress_detail(state: &TargetProgressState) -> String {
    match state {
        TargetProgressState::Queued => "queued".to_owned(),
        TargetProgressState::Running => "running".to_owned(),
        TargetProgressState::Succeeded { detail, .. } => detail.clone(),
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

    /// A poll failing on the server's side: Restate away.
    fn unavailable() -> Fault {
        Fault::Refused("Restate is unavailable".to_owned())
    }

    /// A merge that has landed names the commit it made the moment Restate
    /// reports it, as a short sha linked to the commit on the pull request's
    /// GitHub, beside the words for it; one reported without a commit —
    /// journaled before the commit was kept, or one GitHub named none for —
    /// keeps the words alone. A target whose pull request has no page to go by
    /// names the commit without a link: the host is not known any other way.
    #[test]
    fn a_merged_target_links_the_commit_it_made_by_its_short_sha() {
        let sha = "9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6";
        let mut without_a_page = pr_target(&grouped_row());
        without_a_page.html_url = String::new();
        let mut unknown_commit = pr_target(&serde_row());
        unknown_commit.number = 99;
        let targets = [
            pr_target(&serde_row()),
            without_a_page.clone(),
            unknown_commit.clone(),
        ];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        for target in [&targets[0], &without_a_page] {
            progress.record(
                &target.key(),
                ActionOutcome::Succeeded {
                    detail: "merged".to_owned(),
                    merge_sha: Some(sha.to_owned()),
                },
            );
        }
        progress.record(
            &unknown_commit.key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: None,
            },
        );

        let html = render_drawer(progress, false);

        let serde_page = &serde_row().html_url;
        let repository = serde_page.trim_end_matches("/pull/12");
        assert!(
            html.contains(&format!(
                r#"merged · <a class="commit-link" href="{repository}/commit/{sha}" target="_blank" rel="noreferrer" title="{sha}"><code>9f8e7d6</code></a>"#
            )),
            "the commit is linked under the pull request's repository: {html}"
        );
        assert!(
            html.contains(r#"merged · <code class="commit-ref" title="9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6">9f8e7d6</code>"#),
            "without a page to go by the commit is named unlinked: {html}"
        );
        assert_eq!(
            html.matches(">9f8e7d6</code>").count(),
            2,
            "two merges named their commit, and no more: {html}"
        );
        assert!(
            html.contains("<small>merged</small>"),
            "a merge with no commit named keeps the words alone: {html}"
        );
    }

    /// Whether a retry is offered is [`can_retry`]'s rule, tested with it;
    /// here, that a batch it holds for has the button, and can press it.
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

    /// The listing the projection holds of `batch-1` while it runs: a merge
    /// of twelve, asked for by `alice`, started three minutes before
    /// [`FIXTURE_NOW`].
    fn listed_as_running() -> Listing {
        Listing::Running(RunningBatch {
            batch_id: "batch-1".to_owned(),
            installation_id: 1,
            action: BulkActionKind::Merge,
            requested_by: UserId::new("alice"),
            retried_from: None,
            started_at: FIXTURE_NOW - 3 * 60,
            target_count: 12,
        })
    }

    /// Which of the ways of finding a batch the follow is on, in order. Until
    /// the projection has answered, the batch is being looked up — not asked
    /// of Restate, which is the second word, not the first. Listed as running,
    /// what the listing knows is passed on — what was asked, over how many,
    /// by whom, since when — with Restate asked for the rest. An id the
    /// projection has never heard of is said to be, with why that need not be
    /// the end of it. A projection that could not be read is said to be, and
    /// Restate asked as it always was; the fault itself is the trouble note's.
    #[test]
    fn the_finding_note_says_which_way_of_finding_the_batch_the_follow_is_on() {
        let attaching = Followed::attaching("batch-1", FIXTURE_NOW);
        let with = |listing| Followed {
            listing,
            ..attaching.clone()
        };

        assert_eq!(
            finding_note(&attaching, FIXTURE_NOW),
            "Looking the batch up..."
        );
        assert_eq!(
            finding_note(&with(listed_as_running()), FIXTURE_NOW),
            "Listed as running: merge over 12 pull requests, by alice, started 3m ago. Asking \
             Restate where it stands..."
        );
        assert_eq!(
            finding_note(&with(Listing::Unlisted), FIXTURE_NOW),
            "The projection has no batch by this id, running or finished; one just queued may \
             not be listed yet. Asking Restate where it stands..."
        );
        assert_eq!(
            finding_note(&with(Listing::Unreadable), FIXTURE_NOW),
            "The projection could not be read. Asking Restate where the batch stands..."
        );
    }

    /// Nothing while the batch moved within the minute, nor once it has run
    /// to the end, however long ago. Past the minute, for how long it has
    /// stood and what the dashboard can tell of why. While Restate has not
    /// spoken for a batch the dashboard submitted, the wait is on Restate to
    /// start it. Once it has, the dashboard cannot tell a call being retried
    /// inside its budgets from a service that has died — progress is
    /// published only as verdicts land, so the two look the same from here —
    /// so it says only for how long: not that the batch is lost, which it is
    /// not, nor that the wait is on GitHub, which it cannot know.
    #[test]
    fn the_waiting_note_dates_a_batch_that_stands_still_and_says_what_can_be_told_of_why() {
        let heard = followed(half_done_merge());
        let notice = WAITING_NOTICE_AFTER.as_secs();

        assert_eq!(waiting_note(&heard, FIXTURE_NOW + notice - 1), None);
        let standing = waiting_note(&heard, FIXTURE_NOW + 47 * 60).expect("the batch stands still");
        assert_eq!(
            standing,
            "No progress for 47m. Whether a call is being retried or the service is down, the \
             dashboard cannot tell; one call may take hours inside its budgets, and the batch is \
             durable in Restate for all of them."
        );
        assert!(!standing.to_lowercase().contains("lost"), "{standing}");
        assert!(!standing.contains("GitHub"), "{standing}");

        let queued = Followed::queued(
            "batch-1",
            BulkActionKind::Merge,
            &[pr_target(&grouped_row())],
            FIXTURE_NOW,
        );
        assert_eq!(
            waiting_note(&queued, FIXTURE_NOW + 3 * 60).as_deref(),
            Some(
                "Waiting on Restate to start the batch, 3m after it took it: its service may be \
                 down or deploying, and the batch starts when it is back."
            )
        );

        let finished = followed(finished_with_rejection(RejectReason::NotMergeable));
        assert_eq!(waiting_note(&finished, FIXTURE_NOW + 24 * 3600), None);
    }

    /// Nothing while the polls are answered. While they are not, the fault —
    /// and, once Restate has been heard on the batch, that the batch carries
    /// on and the dashboard keeps asking after it; before Restate has been
    /// heard, the fault alone, since the finding note already says the
    /// dashboard is asking. Once the server has refused the credentials the
    /// dashboard has stopped asking, and the note says so, and that the
    /// reload that signs in again is what brings the follow back — over the
    /// progress last heard, that the batch carries on in Restate meanwhile;
    /// for a batch followed by id alone, not even that, since Restate was
    /// never heard on it. Neither claims to be still asking.
    #[test]
    fn the_trouble_note_passes_the_fault_on_and_says_whether_the_dashboard_is_still_asking() {
        let heard = followed(half_done_merge());
        let attaching = Followed::attaching("batch-1", FIXTURE_NOW);
        let troubled = |trouble, followed: &Followed| Followed {
            trouble: Some(trouble),
            ..followed.clone()
        };

        assert_eq!(trouble_note(&heard), None);
        assert_eq!(trouble_note(&attaching), None);

        assert_eq!(
            trouble_note(&troubled(unavailable(), &heard)).as_deref(),
            Some(
                "Restate is unavailable. The batch carries on in Restate; the dashboard keeps \
                 asking after it."
            )
        );
        assert_eq!(
            trouble_note(&troubled(unavailable(), &attaching)).as_deref(),
            Some("Restate is unavailable")
        );

        let refused_heard =
            trouble_note(&troubled(Fault::SignedOut, &heard)).expect("the poll was refused");
        assert_eq!(
            refused_heard,
            "You are no longer signed in — reload to sign in again. The batch carries on in \
             Restate; the dashboard has stopped asking after it, and picks it up again once the \
             page is reloaded."
        );
        assert!(!refused_heard.contains("keeps asking"), "{refused_heard}");

        let refused_alone =
            trouble_note(&troubled(Fault::SignedOut, &attaching)).expect("the poll was refused");
        assert_eq!(
            refused_alone,
            "You are no longer signed in — reload to sign in again. The dashboard has stopped \
             asking where the batch stands, and asks again once the page is reloaded."
        );
        assert!(!refused_alone.contains("asking after"), "{refused_alone}");
        assert!(
            !refused_alone.contains("carries on in Restate"),
            "{refused_alone}"
        );
    }

    /// Where the notes go on a batch with progress: between the summary and
    /// the rows, the waiting note first as a plain note and the trouble note
    /// after it marked as trouble — over the progress last heard, which stays
    /// on show — and neither while there is nothing to say. What each says is
    /// its function's, tested above.
    #[test]
    fn a_running_batchs_notes_stand_between_the_summary_and_the_rows_over_the_progress_last_heard()
    {
        let troubled = Followed {
            trouble: Some(unavailable()),
            ..followed(half_done_merge())
        };
        let now = FIXTURE_NOW + 47 * 60;
        let waiting = waiting_note(&troubled, now).expect("the batch stands still");
        let trouble = trouble_note(&troubled).expect("the polls are failing");

        let html = render_followed(troubled, now, false);

        assert!(
            html.contains(&format!(
                r#"</div><p class="progress-note">{waiting}</p><p class="progress-note progress-trouble">{trouble}</p><div class="progress-list">"#
            )),
            "{html}"
        );
        assert!(html.contains("merge progress"), "{html}");
        assert!(html.contains("<strong>1/2</strong>"), "{html}");

        let quiet = render_followed(followed(half_done_merge()), FIXTURE_NOW, false);

        assert!(!quiet.contains("progress-note"), "{quiet}");
    }

    /// A batch known by id alone has no progress to summarise: the drawer
    /// names it by its id, says how it is being found where the summary
    /// would be, and offers no retry; the title names the action once the
    /// listing has said it, as it does once the progress is in. Once the
    /// server has refused the credentials the dashboard is no longer finding
    /// the batch, and the drawer does not say it is: the finding note goes,
    /// and the trouble note stands alone.
    #[test]
    fn a_batch_followed_by_id_alone_says_how_it_is_being_found_in_place_of_the_summary_until_signed_out()
     {
        let attaching = Followed::attaching("batch-1", FIXTURE_NOW);

        let html = render_followed(attaching.clone(), FIXTURE_NOW, false);

        assert!(html.contains("Batch batch-1"), "{html}");
        assert!(html.contains("Batch progress"), "{html}");
        assert!(
            html.contains(&format!(
                r#"<p class="progress-note">{}</p>"#,
                finding_note(&attaching, FIXTURE_NOW)
            )),
            "{html}"
        );
        assert!(!html.contains("progress-summary"), "{html}");
        assert!(!html.contains(RETRY_BUTTON), "{html}");

        let listed = Followed {
            listing: listed_as_running(),
            ..attaching.clone()
        };

        let html = render_followed(listed.clone(), FIXTURE_NOW, false);

        assert!(html.contains("merge progress"), "{html}");
        assert!(
            html.contains(&format!(
                r#"<p class="progress-note">{}</p>"#,
                finding_note(&listed, FIXTURE_NOW)
            )),
            "{html}"
        );
        assert!(!html.contains("progress-summary"), "{html}");

        let refused = Followed {
            trouble: Some(Fault::SignedOut),
            ..attaching
        };
        let trouble = trouble_note(&refused).expect("the poll was refused");

        let html = render_followed(refused, FIXTURE_NOW, false);

        assert!(
            !html.contains(r#"<p class="progress-note">"#),
            "the finding note goes: {html}"
        );
        assert!(
            html.contains(&format!(
                r#"<p class="progress-note progress-trouble">{trouble}</p>"#
            )),
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
