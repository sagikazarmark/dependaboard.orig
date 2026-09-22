//! The bulk action the dashboard is following: the pill that reports its
//! progress, the drawer that opens from the pill, the ways a batch becomes
//! the followed one — confirmed here, named in the URL, picked from the audit
//! view, or a finished one's rejected targets queued again — and what is said
//! when the follow ends.
//!
//! One batch is followed at a time. Following another, however it comes, ends
//! the follow before it, so two never report into the one pill.

use dependaboard_core::{BatchProgress, BatchReceipt, new_batch_id, unix_seconds};
use dioxus::core::Task;
use dioxus::prelude::*;

use crate::components::toast::{ToastOptions, Toasts, use_toast};
use crate::ui::batch::{
    ATTACH_TIMEOUT, BatchOutcome, Followed, Listing, SUBMIT_ATTEMPTS, ServerBatch, ServerFollow,
    follow_batch, run_batch,
};
use crate::ui::dashboard_state::{Connection, DashboardState, use_dashboard};
use crate::ui::format::{relative_time, verdict_tally};
use crate::ui::progress_drawer::ProgressDrawer;
use crate::ui::retry::{LeftOut, ServerRefresh, left_out_notice, refresh_rejected};
use crate::ui::{Fault, PendingAction, SIGNED_OUT_MESSAGE, sticky};

/// Where a followed batch lands: what is known of it, which the pill and
/// drawer read, the task following it, whether the drawer is showing, the
/// toasts its outcome goes to, and the dashboard it reloads once it has run.
#[derive(Clone, Copy)]
pub(crate) struct BatchHost {
    pub(crate) followed: Signal<Option<Followed>>,
    pub(crate) follower: Signal<Option<Task>>,
    pub(crate) open: Signal<bool>,
    pub(crate) toast: Toasts,
    pub(crate) state: DashboardState,
}

impl BatchHost {
    /// Makes `followed` the batch the host follows and `follower` the task
    /// following it, in place of whatever was followed before, and opens the
    /// drawer on it.
    fn take_over(&mut self, followed: Followed, follower: impl Future<Output = ()> + 'static) {
        if let Some(previous) = self.follower.take() {
            previous.cancel();
        }
        self.followed.set(Some(followed));
        self.open.set(true);
        let task = spawn(follower);
        self.follower.set(Some(task));
    }

    /// Whether the host is following the batch `batch_id` names.
    fn follows(&self, batch_id: &str) -> bool {
        self.followed
            .peek()
            .as_ref()
            .is_some_and(|followed| followed.batch_id == batch_id)
    }
}

/// The pill and drawer for `followed`; nothing while no batch is followed.
/// `follower` is the task following it and `open` says whether the drawer is
/// showing.
///
/// The drawer's **Retry rejected** is handled here: the finished batch's
/// rejected targets are refreshed and queued as a new batch of the same
/// kind, which takes the drawer over as any queued batch does.
#[component]
pub(crate) fn ActiveBatch(
    followed: Signal<Option<Followed>>,
    follower: Signal<Option<Task>>,
    mut open: Signal<bool>,
) -> Element {
    let host = BatchHost {
        followed,
        follower,
        open,
        toast: use_toast(),
        state: use_dashboard(),
    };
    let retrying = use_signal(|| false);
    // Read here, not only for the drawer: the pill flips to waiting on the
    // clock's tick, and it has to hear the tick with the drawer closed.
    let now = host.state.now();
    let current = followed.read();
    let Some(current) = &*current else {
        return rsx! {};
    };
    let (dot, label) = match &current.progress {
        Some(progress) => {
            let count = format!(
                "{}: {}/{}",
                progress.action,
                progress.settled(),
                progress.targets.len()
            );
            if progress.completed {
                ("progress-live complete", count)
            } else if current.signed_out() {
                ("progress-live waiting", format!("{count} · signed out"))
            } else if current.stands_still(now) {
                let standing = relative_time(now, current.since);
                (
                    "progress-live waiting",
                    format!("{count} · no progress {standing}"),
                )
            } else {
                ("progress-live", count)
            }
        }
        None if current.signed_out() => ("progress-live waiting", "batch: signed out".to_owned()),
        None => match current.listing.action() {
            Some(action) => ("progress-live", format!("{action}: following...")),
            None => ("progress-live", "batch: following...".to_owned()),
        },
    };
    rsx! {
        button {
            class: "progress-pill",
            onclick: move |_| open.toggle(),
            span { class: dot }
            "{label}"
        }
        if open() {
            ProgressDrawer {
                followed: current.clone(),
                now,
                retrying: retrying(),
                onretry: move |_| {
                    let finished = followed
                        .peek()
                        .as_ref()
                        .and_then(|followed| followed.progress.clone());
                    if let Some(finished) = finished {
                        retry_rejected(finished, retrying, host);
                    }
                },
                onclose: move |_| open.set(false),
            }
        }
    }
}

/// Queues `pending` as a bulk action and follows it to the end: the host's
/// followed batch carries each poll's answer and its drawer opens on it, the
/// outcome becomes a toast, and a batch that ran to completion reloads the
/// page. Once Restate has the batch its rows leave the selection and any
/// target the server left out is named; a submission Restate never took
/// leaves the selection as it was, so the user has the rows to try again.
pub(crate) fn queue_batch(pending: PendingAction, mut host: BatchHost) {
    let action = pending.action;
    let targets = pending.targets();
    let retried_from = pending.retried_from.clone();
    let batch_id = new_batch_id();
    let followed = Followed::queued(&batch_id, action, &targets, unix_seconds());
    host.take_over(followed.clone(), async move {
        let mut batch = ServerBatch {
            follow: ServerFollow {
                batch_id,
                state: host.state,
            },
            action,
            targets,
            retried_from,
        };
        let mut followed_signal = host.followed;
        let outcome = run_batch(
            &mut batch,
            followed,
            |update| {
                followed_signal.set(Some(update.clone()));
            },
            |receipt| account_for(&pending, &receipt, host),
        )
        .await;
        conclude(outcome, host);
    });
}

/// Accounts for `pending` once Restate has the batch it was queued as. Its
/// rows leave the selection — the ones queued and the ones left out alike,
/// since neither is for a next batch — and the targets the server left out,
/// pull requests the projection no longer had by the time the batch was
/// submitted, are named with their repositories, as the retry names the
/// targets it leaves out.
fn account_for(pending: &PendingAction, receipt: &BatchReceipt, mut host: BatchHost) {
    host.state.deselect(&pending.rows);
    let left_out: Vec<LeftOut> = pending
        .targets()
        .iter()
        .filter(|target| receipt.left_out.contains(&target.key()))
        .map(LeftOut::no_longer_open)
        .collect();
    if let Some(notice) = left_out_notice("the batch", &left_out) {
        host.toast.warning(notice, sticky());
    }
}

/// Follows the batch `batch_id` names, known by its id alone — from the URL
/// after a reload, or picked from the audit view — to the end, as
/// [`queue_batch`] follows one it queued. The projection is asked first: a
/// batch it holds finished opens from the record at once, without a poll; one
/// it lists as running is polled for as long as it takes; only an id it has
/// never heard of is left to Restate to vouch for, within [`ATTACH_TIMEOUT`].
/// Following the batch already followed changes nothing. A batch found
/// already finished is shown as it stands and not announced: it was announced
/// when it finished, and the page has nothing to reload for it.
pub(crate) fn attach_batch(batch_id: String, mut host: BatchHost) {
    if host.follows(&batch_id) {
        return;
    }
    let followed = Followed::attaching(&batch_id, unix_seconds());
    host.take_over(followed.clone(), async move {
        let mut batch = ServerFollow {
            batch_id,
            state: host.state,
        };
        let mut seen_running = false;
        let outcome = follow_batch(&mut batch, followed, |update| {
            seen_running |= update.heard && !update.completed();
            host.followed.set(Some(update.clone()));
        })
        .await;
        if !seen_running && matches!(outcome, BatchOutcome::Completed(_)) {
            return;
        }
        conclude(outcome, host);
    });
}

/// Says how the follow ended. A batch that ran to the end is announced and
/// the page reloaded. One Restate would not take, or has no progress for, is
/// no batch to follow: the pill and drawer stand down, which also takes it
/// out of the URL. A follow the server refused the credentials of stands as
/// it was — the drawer says why, and the batch stays in the URL for the
/// reload to pick up — and the page is told it is signed out, as it is when
/// the submission itself was refused, so the banner goes up at once and the
/// live refresh asks nothing more either.
fn conclude(outcome: BatchOutcome, mut host: BatchHost) {
    match outcome {
        BatchOutcome::Completed(progress) => {
            toast_completion(&host.toast, &progress);
            host.state.reload();
        }
        BatchOutcome::NotSubmitted(fault) => {
            host.toast.error(not_submitted_notice(&fault), sticky());
            host.followed.set(None);
            if fault == Fault::SignedOut {
                host.state.poll_missed(Connection::SignedOut);
            }
        }
        BatchOutcome::Unknown => {
            let notice = host
                .followed
                .peek()
                .as_ref()
                .map_or_else(String::new, unknown_notice);
            host.toast.warning(notice, sticky());
            host.followed.set(None);
        }
        BatchOutcome::SignedOut => host.state.poll_missed(Connection::SignedOut),
    }
}

/// What the toast says of a batch nobody would vouch for, `followed` being
/// the follow as it ended: the projection had no record of it and Restate no
/// progress, which is a stale or foreign link — or, when the projection could
/// not be read, Restate's word alone, with the audit view left as the place a
/// finished batch would still be. A follow the projection listed as running,
/// or never answered, does not end this way: the one is waited for however
/// long, the other is answered before a poll is taken; the words are Restate's
/// alone all the same.
fn unknown_notice(followed: &Followed) -> String {
    let batch_id = &followed.batch_id;
    let seconds = ATTACH_TIMEOUT.as_secs();
    match followed.listing {
        Listing::Unlisted => format!(
            "No batch {batch_id}: the projection has no record of it, running or finished, and Restate has had no progress for it in {seconds} seconds of asking. The link is stale, or from another deployment."
        ),
        Listing::Unreadable => format!(
            "Restate has no progress for batch {batch_id} after {seconds} seconds, and the projection could not be read: it may never have run, or its workflow has been retired since. A finished batch is under Batches."
        ),
        Listing::Unasked | Listing::Running(_) => format!(
            "Restate has no progress for batch {batch_id} after {seconds} seconds: it may never have run, or its workflow has been retired since. A finished batch is under Batches."
        ),
    }
}

/// What the toast says of a batch Restate never took, `fault` being the
/// submission's last: how often it was tried, unless the credentials were
/// refused, in which case it was tried once and asking again would only
/// have prompted for them again.
fn not_submitted_notice(fault: &Fault) -> String {
    match fault {
        Fault::SignedOut => format!("Batch was not submitted: {fault}"),
        _ => format!("Batch was not submitted after {SUBMIT_ATTEMPTS} attempts: {fault}"),
    }
}

/// Retries what `finished` rejected: each rejected target is synced again so
/// the new batch carries its current head SHA, the ones there is no retrying
/// are named in a toast, and the rest are queued as a new batch of the same
/// kind that names `finished` as the batch it retries, which replaces
/// `finished` in the host and so in the drawer. `retrying` is held while the
/// targets are being refreshed.
///
/// The refresh can take a while, and the user may confirm another batch in
/// the meantime; a retry that finds the host following a batch other than
/// `finished` stands down rather than take the drawer from the one running.
/// A retry the server refused the credentials of is given up whole, saying
/// so, and the page is told it is signed out, as it is when a follow is
/// refused: the banner goes up at once and the live refresh asks nothing
/// more either.
fn retry_rejected(finished: BatchProgress, mut retrying: Signal<bool>, mut host: BatchHost) {
    if retrying() {
        return;
    }
    retrying.set(true);
    spawn(async move {
        let refreshed = refresh_rejected(&ServerRefresh { state: host.state }, &finished).await;
        retrying.set(false);
        let Ok(refreshed) = refreshed else {
            host.toast.error(
                format!("The retry was given up: {SIGNED_OUT_MESSAGE}"),
                sticky(),
            );
            host.state.poll_missed(Connection::SignedOut);
            return;
        };
        if let Some(notice) = refreshed.notice() {
            host.toast.warning(notice, sticky());
        }
        let Some(retry) = refreshed.retry() else {
            return;
        };
        if !host.follows(&finished.batch_id) {
            host.toast.info(
                "The retry stood down: another batch was queued while its targets were being refreshed."
                    .to_owned(),
                sticky(),
            );
            return;
        }
        queue_batch(retry, host);
    });
}

/// Announces a finished batch. A batch always runs to the end; what varies is
/// whether every target settled cleanly, in which case that is all there is
/// to say, or some were rejected or failed, in which case the toast carries
/// the tally and stays until it is read: the drawer has the reasons, and the
/// user has to go and look.
fn toast_completion(toast: &Toasts, progress: &BatchProgress) {
    match completion_notice(progress) {
        None => toast.success("Batch complete".to_owned(), ToastOptions::new()),
        Some(notice) => toast.warning(notice, sticky()),
    }
}

/// What the completion toast says past a clean "Batch complete", if anything:
/// nothing while every target succeeded; otherwise the whole verdict tally,
/// in the drawer's words. A rejected target counts as much as a failed one —
/// GitHub said no to the request as sent, which is not what the user asked
/// for — but is not called a failure, and the drawer's summary reads the
/// same. A batch rejected whole falls under the same rule as one rejected in
/// part: the tally says so, and no inference about a shared cause is made
/// from the counts, as none is made from the verdicts.
fn completion_notice(progress: &BatchProgress) -> Option<String> {
    if progress.rejected == 0 && progress.failed == 0 {
        return None;
    }
    Some(format!(
        "Batch complete: {}. Open the batch for their reasons.",
        verdict_tally(progress.succeeded, progress.rejected, progress.failed)
    ))
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::time::Duration;

    use dependaboard_core::{ActionOutcome, BulkActionKind, PrRecord, RejectReason};
    use dioxus::core::consume_context_from_scope;

    use super::*;
    use crate::ui::batch::WAITING_NOTICE_AFTER;
    use crate::ui::pr_target;
    use crate::ui::test_support::{
        BATCH, DashboardFixture, FIXTURE_NOW, followed, grouped_row, half_done_merge, off_page_row,
        render, serde_row,
    };

    #[test]
    fn nothing_shows_while_no_batch_is_followed() {
        fn Fixture() -> Element {
            let followed = use_signal(|| None);
            let follower = use_signal(|| None);
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { ActiveBatch { followed, follower, open } }
            }
        }
        let html = render(Fixture);

        assert!(!html.contains("progress-pill"), "{html}");
        assert!(!html.contains("progress-drawer"), "{html}");
    }

    #[test]
    fn the_pill_counts_the_batch_and_keeps_the_drawer_closed_until_asked() {
        fn Fixture() -> Element {
            let followed = use_signal(|| Some(followed(half_done_merge())));
            let follower = use_signal(|| None);
            let open = use_signal(|| false);
            rsx! {
                DashboardFixture { ActiveBatch { followed, follower, open } }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains(r#"<span class="progress-live"></span>merge: 1/2"#),
            "{html}"
        );
        assert!(!html.contains("no progress"), "{html}");
        assert!(!html.contains("progress-drawer"), "{html}");
    }

    /// The pill is what shows while the drawer is closed, so it is the pill
    /// that has to say the batch has stopped moving: from the drawer's notice
    /// on, the count carries how long, and the dot stops throbbing. It does
    /// not say the batch is stalled or lost — it is neither, as far as anyone
    /// can tell — and it flips on the dashboard's clock, which it reads
    /// whether or not the drawer is open.
    #[test]
    fn a_pill_whose_batch_has_stood_still_says_for_how_long_and_stops_throbbing() {
        fn Fixture() -> Element {
            let followed = use_signal(|| Some(followed(half_done_merge())));
            let follower = use_signal(|| None);
            let open = use_signal(|| false);
            rsx! {
                DashboardFixture { now: FIXTURE_NOW + WAITING_NOTICE_AFTER.as_secs(),
                    ActiveBatch { followed, follower, open }
                }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains(
                r#"<span class="progress-live waiting"></span>merge: 1/2 · no progress 1m"#
            ),
            "{html}"
        );
        assert!(!html.to_lowercase().contains("stall"), "{html}");
        assert!(!html.to_lowercase().contains("lost"), "{html}");
    }

    /// The throb says the batch is being followed. Once the server has
    /// refused the credentials it is not, and the pill — what shows with the
    /// drawer closed — says so beside the count last heard and stops
    /// throbbing, rather than stand over a follow that has ended as if it
    /// were live.
    #[test]
    fn a_pill_whose_follow_was_refused_the_credentials_says_so_and_stops_throbbing() {
        fn Fixture() -> Element {
            let followed = use_signal(|| {
                Some(Followed {
                    trouble: Some(Fault::SignedOut),
                    ..followed(half_done_merge())
                })
            });
            let follower = use_signal(|| None);
            let open = use_signal(|| false);
            rsx! {
                DashboardFixture { ActiveBatch { followed, follower, open } }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains(r#"<span class="progress-live waiting"></span>merge: 1/2 · signed out"#),
            "{html}"
        );
    }

    /// A finished batch stood still for good; that is not a wait, and the
    /// pill marks it complete however long ago it finished.
    #[test]
    fn an_open_drawer_follows_the_batch_and_a_finished_one_marks_the_pill() {
        fn Fixture() -> Element {
            let followed = use_signal(|| {
                let mut progress = half_done_merge();
                progress.record_failure(
                    &pr_target(&serde_row()).key(),
                    "GitHub mutation failed with HTTP 500: Internal Server Error",
                );
                Some(followed(progress))
            });
            let follower = use_signal(|| None);
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { now: FIXTURE_NOW + 24 * 3600,
                    ActiveBatch { followed, follower, open }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains(r#"class="progress-live complete""#), "{html}");
        assert!(
            html.contains("merge: 2/2<"),
            "the pill counts the failed target too, and says nothing of a wait: {html}"
        );
        assert!(html.contains("merge progress"), "{html}");
        assert!(html.contains("1 succeeded, 0 rejected, 1 failed"), "{html}");
        assert!(
            html.contains("GitHub mutation failed with HTTP 500: Internal Server Error"),
            "the failed target's row shows its reason: {html}"
        );
    }

    /// After a reload the dashboard follows the batch its URL names by id
    /// alone: the pill is up and the drawer open on it before anything has
    /// answered, so the operator sees at once that the batch is being found.
    #[test]
    fn a_batch_followed_by_id_alone_shows_a_pill_and_an_open_drawer_asking_after_it() {
        fn Fixture() -> Element {
            let followed = use_signal(|| Some(Followed::attaching("batch-1", FIXTURE_NOW)));
            let follower = use_signal(|| None);
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { ActiveBatch { followed, follower, open } }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains(r#"<span class="progress-live"></span>batch: following..."#),
            "{html}"
        );
        assert!(html.contains("Batch batch-1"), "{html}");
        assert!(html.contains("Looking the batch up..."), "{html}");
    }

    /// The retry needs the toasts and the dashboard around the drawer, which
    /// is what this component gives it; so it is here that the drawer's offer
    /// is mounted with them in place.
    #[test]
    fn an_open_drawer_on_a_finished_batch_with_a_rejection_offers_the_retry() {
        fn Fixture() -> Element {
            let followed = use_signal(|| {
                let mut progress = half_done_merge();
                progress.record(
                    &pr_target(&serde_row()).key(),
                    ActionOutcome::Rejected {
                        reason: RejectReason::NotMergeable,
                    },
                );
                Some(followed(progress))
            });
            let follower = use_signal(|| None);
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { ActiveBatch { followed, follower, open } }
            }
        }
        let html = render(Fixture);

        assert!(html.contains(">Retry rejected<"), "{html}");
    }

    /// [`half_done_merge`] with the second target settled by `outcome`.
    fn finished(outcome: ActionOutcome) -> BatchProgress {
        let mut progress = half_done_merge();
        progress.record(&pr_target(&serde_row()).key(), outcome);
        progress
    }

    /// [`half_done_merge`] with every target rejected: the first's verdict
    /// replaced, as the worst case of a rebase submitted without a user token.
    fn all_rejected() -> BatchProgress {
        let targets = [pr_target(&grouped_row()), pr_target(&serde_row())];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        for target in &targets {
            progress.record(
                &target.key(),
                ActionOutcome::Rejected {
                    reason: RejectReason::NoUserToken,
                },
            );
        }
        progress
    }

    /// Three targets, one to each terminal column.
    fn one_of_each() -> BatchProgress {
        let mut third = grouped_row();
        third.number = 77;
        let targets = [
            pr_target(&grouped_row()),
            pr_target(&serde_row()),
            pr_target(&third),
        ];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: None,
            },
        );
        progress.record(
            &targets[1].key(),
            ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            },
        );
        progress.record_failure(
            &targets[2].key(),
            "GitHub mutation failed with HTTP 500: Internal Server Error",
        );
        progress
    }

    /// A batch every target of which settled cleanly is announced as
    /// complete and nothing more; one with a rejected or a failed target is
    /// announced with the whole tally, in the drawer's words, so a rejection
    /// — GitHub said no to the request as sent — is not congratulated as a
    /// success, and is not called a failure either. Every tally is a warning
    /// that stays: the user has to go and read the reasons.
    #[test]
    fn a_finished_batch_is_announced_clean_or_with_its_whole_tally() {
        assert_eq!(
            completion_notice(&finished(ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: None,
            })),
            None
        );
        assert_eq!(
            completion_notice(&finished(ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            })),
            Some(
                "Batch complete: 1 succeeded, 1 rejected, 0 failed. Open the batch for their reasons."
                    .to_owned()
            )
        );
        let mut one_failed = half_done_merge();
        one_failed.record_failure(
            &pr_target(&serde_row()).key(),
            "GitHub mutation failed with HTTP 500: Internal Server Error",
        );
        assert_eq!(
            completion_notice(&one_failed),
            Some(
                "Batch complete: 1 succeeded, 0 rejected, 1 failed. Open the batch for their reasons."
                    .to_owned()
            )
        );
        assert_eq!(
            completion_notice(&one_of_each()),
            Some(
                "Batch complete: 1 succeeded, 1 rejected, 1 failed. Open the batch for their reasons."
                    .to_owned()
            )
        );
        assert_eq!(
            completion_notice(&all_rejected()),
            Some(
                "Batch complete: 0 succeeded, 2 rejected, 0 failed. Open the batch for their reasons."
                    .to_owned()
            ),
            "a batch rejected whole is announced by the same rule as one rejected in part"
        );
    }

    /// A submission that kept failing is announced with how often it was
    /// tried; one the server refused the credentials of was tried once —
    /// trying again would only prompt for them again — and the toast does
    /// not claim otherwise.
    #[test]
    fn a_batch_not_submitted_says_how_often_it_was_tried_unless_the_credentials_were_refused() {
        assert_eq!(
            not_submitted_notice(&Fault::Refused("Restate is unavailable".to_owned())),
            "Batch was not submitted after 5 attempts: Restate is unavailable"
        );
        assert_eq!(
            not_submitted_notice(&Fault::SignedOut),
            "Batch was not submitted: You are no longer signed in — reload to sign in again"
        );
    }

    // The tests below record what these five functions do today, ahead of the
    // reseaming that will put them behind a gateway and let them be tested as
    // decisions. Where a toast is the only thing a decision leaves behind,
    // they read the rendered toast region; that assertion is scaffolding for
    // the seam to replace, not a considered choice of what to assert on.

    /// What a mounted [`Harness`] does with the host it built. It is called
    /// from the component that built it, so the signal writes and the
    /// `spawn`s the functions under test make have a live scope around them.
    ///
    /// A props field has to compare, and function pointers do not compare
    /// meaningfully; a harness is mounted once and never rediffed against
    /// another, so every action counts as the one already mounted.
    #[derive(Clone, Copy)]
    struct Act(fn(BatchHost, Seen));

    impl PartialEq for Act {
        fn eq(&self, _: &Self) -> bool {
            true
        }
    }

    /// The host's own signals, whether a retry is in flight, how often the
    /// page was reloaded, and the host itself once the driver has built it —
    /// provided on the app scope, so a test can read them from outside the
    /// runtime once the action has run.
    #[derive(Clone, Copy)]
    struct Seen {
        followed: Signal<Option<Followed>>,
        open: Signal<bool>,
        retrying: Signal<bool>,
        reloads: Signal<u32>,
        host: Signal<Option<BatchHost>>,
    }

    /// A [`BatchHost`] under the toast provider and the dashboard state, with
    /// `act` run against it once, as the confirmation's and the drawer's
    /// handlers run against the one [`ActiveBatch`] builds.
    #[component]
    fn Harness(
        #[props(default)] followed: Option<Followed>,
        #[props(default = Connection::Online)] connection: Connection,
        #[props(default)] selected: Vec<PrRecord>,
        act: Act,
    ) -> Element {
        let seen = use_context_provider(|| Seen {
            followed: Signal::new(followed),
            open: Signal::new(false),
            retrying: Signal::new(false),
            reloads: Signal::new(0),
            host: Signal::new(None),
        });
        rsx! {
            DashboardFixture {
                connection,
                selected,
                reloads: seen.reloads,
                Driver { act }
            }
        }
    }

    /// Builds the host where the toasts and the dashboard are, and runs the
    /// action on it.
    #[component]
    fn Driver(act: Act) -> Element {
        let mut seen = use_context::<Seen>();
        let host = BatchHost {
            followed: seen.followed,
            follower: use_signal(|| None),
            open: seen.open,
            toast: use_toast(),
            state: use_dashboard(),
        };
        use_hook(move || {
            seen.host.set(Some(host));
            (act.0)(host, seen);
        });
        rsx! {}
    }

    /// The harness with nothing followed, nothing selected, and the line to
    /// the server online.
    fn harness(act: fn(BatchHost, Seen)) -> HarnessProps {
        HarnessProps {
            followed: None,
            connection: Connection::Online,
            selected: Vec::new(),
            act: Act(act),
        }
    }

    /// Mounts the harness and runs its action.
    fn mount(props: HarnessProps) -> (VirtualDom, Seen) {
        let mut dom = VirtualDom::new_with_props(Harness, props);
        dom.rebuild_in_place();
        let seen = dom
            .in_runtime(|| consume_context_from_scope::<Seen>(ScopeId::APP))
            .expect("the harness provides what the host left");
        (dom, seen)
    }

    /// Renders every scope the action left dirty, and the ones their writes
    /// dirty in turn: a toast is sent into a signal the provider reads, one
    /// level above where it was sent from.
    fn settle(dom: &mut VirtualDom) {
        for _ in 0..8 {
            dom.render_immediate_to_vec();
        }
    }

    /// Runs the task the action spawned to its end. Every flow driven here is
    /// refused the credentials before it would reach the server, so it ends
    /// without a poll, a sleep, or a server function being called.
    ///
    /// The dom polls its tasks while it waits for work, and a flow that ends
    /// without dirtying a component leaves it waiting for good, so each wait
    /// is bounded rather than waited on. A task that did not run shows up in
    /// the assertions, not here.
    async fn drive(dom: &mut VirtualDom) {
        for _ in 0..4 {
            let _ = tokio::time::timeout(Duration::from_millis(20), dom.wait_for_work()).await;
            settle(dom);
        }
    }

    /// The toast region as it stands, which is where a decision whose only
    /// mark is a toast can be read.
    fn toasts(dom: &mut VirtualDom) -> String {
        settle(dom);
        dioxus::ssr::render(dom)
    }

    /// The host the driver built, as the action left it.
    fn host(dom: &VirtualDom, seen: Seen) -> BatchHost {
        dom.in_runtime(|| (*seen.host.peek()).expect("the driver published the host"))
    }

    /// The line to the server as the action left it.
    fn connection(dom: &VirtualDom, seen: Seen) -> Connection {
        let host = host(dom, seen);
        dom.in_runtime(|| host.state.connection())
    }

    /// The batch followed as the action left it.
    fn following(dom: &VirtualDom, seen: Seen) -> Option<Followed> {
        dom.in_runtime(|| seen.followed.peek().clone())
    }

    /// A finished merge with one target rejected over a head that moved,
    /// which is a rejection a retry can cure.
    fn with_a_curable_rejection() -> BatchProgress {
        finished(ActionOutcome::Rejected {
            reason: RejectReason::StaleSha {
                expected: "def456".to_owned(),
                actual: "def457".to_owned(),
            },
        })
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// A batch that ran to the end is announced and the page reloaded, and
    /// the follow is left standing: the drawer stays open on the finished
    /// batch, and the URL keeps it.
    #[test]
    fn a_batch_that_ran_to_the_end_is_announced_and_reloads_the_page_with_the_follow_left_standing()
    {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            ..harness(|host, _| conclude(BatchOutcome::Completed(one_of_each()), host))
        });

        assert_eq!(dom.in_runtime(|| *seen.reloads.peek()), 1);
        assert!(following(&dom, seen).is_some(), "the follow stands");
        assert_eq!(connection(&dom, seen), Connection::Online);
        let html = toasts(&mut dom);
        assert!(html.contains(r#"data-type="warning""#), "{html}");
        assert!(
            html.contains("Batch complete: 1 succeeded, 1 rejected, 1 failed."),
            "{html}"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// A batch Restate would not take is no batch to follow: the pill and
    /// drawer stand down, which takes it out of the URL too, and the page is
    /// not reloaded — nothing ran. The line to the server is left as it was:
    /// Restate not answering is not the browser being signed out.
    #[test]
    fn a_batch_restate_would_not_take_stands_the_follow_down_without_a_reload_or_a_sign_out() {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            ..harness(|host, _| {
                conclude(
                    BatchOutcome::NotSubmitted(Fault::Refused("Restate is unavailable".to_owned())),
                    host,
                );
            })
        });

        assert_eq!(following(&dom, seen), None, "the follow stood down");
        assert_eq!(dom.in_runtime(|| *seen.reloads.peek()), 0);
        assert_eq!(connection(&dom, seen), Connection::Online);
        let html = toasts(&mut dom);
        assert!(html.contains(r#"data-type="error""#), "{html}");
        assert!(
            html.contains("Batch was not submitted after 5 attempts: Restate is unavailable"),
            "{html}"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// The submission being refused the credentials is the one fault that
    /// both stands the follow down — there is no batch to pick up on the
    /// reload, so nothing should stay in the URL — and signs the page out, so
    /// the banner goes up at once and the live refresh asks nothing more. It
    /// is the pair of the refused *follow* below, which does the second and
    /// not the first.
    #[test]
    fn a_submission_the_credentials_were_refused_of_stands_the_follow_down_and_signs_the_page_out()
    {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            ..harness(|host, _| conclude(BatchOutcome::NotSubmitted(Fault::SignedOut), host))
        });

        assert_eq!(following(&dom, seen), None, "the follow stood down");
        assert_eq!(connection(&dom, seen), Connection::SignedOut);
        assert_eq!(dom.in_runtime(|| *seen.reloads.peek()), 0);
        let html = toasts(&mut dom);
        assert!(
            html.contains("Batch was not submitted: You are no longer signed in"),
            "{html}"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// A follow the server refused the credentials of stands as it was: the
    /// batch is still running in Restate, so it stays followed and stays in
    /// the URL for the reload to pick up. The page is signed out all the
    /// same, and nothing is said in a toast — the drawer says it.
    #[test]
    fn a_follow_the_credentials_were_refused_of_signs_the_page_out_and_leaves_the_follow_standing()
    {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            ..harness(|host, _| conclude(BatchOutcome::SignedOut, host))
        });

        assert!(
            following(&dom, seen).is_some(),
            "the batch stays followed, so the URL keeps it across the reload"
        );
        assert_eq!(connection(&dom, seen), Connection::SignedOut);
        assert_eq!(dom.in_runtime(|| *seen.reloads.peek()), 0);
        let html = toasts(&mut dom);
        assert!(html.contains(r#"aria-label="0 notifications""#), "{html}");
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// A batch nobody would vouch for stands the follow down with the
    /// projection's word on it in the notice, read off the follow as it
    /// ended — before it is taken down. The page is neither reloaded nor
    /// signed out.
    #[test]
    fn a_batch_nobody_would_vouch_for_stands_the_follow_down_and_says_which_link_it_was() {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(Followed {
                listing: Listing::Unlisted,
                ..followed(half_done_merge())
            }),
            ..harness(|host, _| conclude(BatchOutcome::Unknown, host))
        });

        assert_eq!(following(&dom, seen), None, "the follow stood down");
        assert_eq!(dom.in_runtime(|| *seen.reloads.peek()), 0);
        assert_eq!(connection(&dom, seen), Connection::Online);
        let html = toasts(&mut dom);
        assert!(html.contains(r#"data-type="warning""#), "{html}");
        assert!(
            html.contains("No batch batch-1: the projection has no record of it"),
            "the notice is read off the follow before it is taken down: {html}"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// Queuing a batch takes the drawer over at once, before anything has
    /// been submitted: the pill and drawer show the dashboard's own snapshot
    /// of what was queued, in place of whatever was followed before. From a
    /// page the server has already refused, the submission is not made at
    /// all, and the batch is reported as not submitted for that reason.
    #[tokio::test]
    async fn queuing_a_batch_takes_the_drawer_over_and_a_refused_submission_hands_it_back() {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            connection: Connection::SignedOut,
            ..harness(|host, _| {
                queue_batch(
                    PendingAction {
                        action: BulkActionKind::Merge,
                        rows: vec![grouped_row()],
                        retried_from: None,
                    },
                    host,
                );
            })
        });

        let queued = following(&dom, seen).expect("the queued batch is followed at once");
        assert_ne!(
            queued.batch_id, "batch-1",
            "it took over from the batch before it"
        );
        assert!(!queued.heard, "Restate has not spoken for it yet");
        assert_eq!(
            queued
                .progress
                .as_ref()
                .map(|progress| progress.targets.len()),
            Some(1),
            "the drawer shows the row queued before the first answer"
        );
        assert!(
            dom.in_runtime(|| *seen.open.peek()),
            "the drawer opened on it"
        );

        drive(&mut dom).await;

        assert_eq!(following(&dom, seen), None, "the follow stood down");
        let html = toasts(&mut dom);
        assert!(
            html.contains("Batch was not submitted: You are no longer signed in"),
            "{html}"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// Following the batch already followed changes nothing: it does not
    /// restart the follow, throw away the progress in hand, or reopen a
    /// drawer the user has closed.
    #[test]
    fn following_the_batch_already_followed_leaves_it_and_the_closed_drawer_as_they_were() {
        let (dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            ..harness(|host, _| attach_batch("batch-1".to_owned(), host))
        });

        assert_eq!(
            following(&dom, seen),
            Some(followed(half_done_merge())),
            "the follow in hand is left alone, progress and all"
        );
        assert!(
            !dom.in_runtime(|| *seen.open.peek()),
            "the drawer is not reopened"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// Following another batch by id takes the drawer over from the one
    /// followed and asks after the new one. From a page the server has
    /// already refused nothing is asked, and the follow ends signed out: it
    /// stands as it was, so the reload picks the batch up from the URL, and
    /// nothing is toasted.
    #[tokio::test]
    async fn following_another_batch_by_id_takes_the_drawer_over_and_a_refused_follow_stands() {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            connection: Connection::SignedOut,
            ..harness(|host, _| attach_batch(BATCH.to_owned(), host))
        });

        let attaching = following(&dom, seen).expect("the new batch is followed at once");
        assert_eq!(attaching.batch_id, BATCH);
        assert_eq!(attaching.progress, None, "nothing is known of it yet");
        assert!(
            dom.in_runtime(|| *seen.open.peek()),
            "the drawer opened on it"
        );

        drive(&mut dom).await;

        let stood = following(&dom, seen).expect("a refused follow stands as it was");
        assert_eq!(stood.batch_id, BATCH);
        assert!(stood.signed_out(), "the drawer says why it stopped asking");
        assert_eq!(connection(&dom, seen), Connection::SignedOut);
        let html = toasts(&mut dom);
        assert!(html.contains(r#"aria-label="0 notifications""#), "{html}");
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// Once Restate has the batch, every row the action was queued with
    /// leaves the selection — the ones it runs and the ones the server left
    /// out alike, since neither is for a next batch — and the left-out ones
    /// are named with their repositories, in the retry's words.
    #[test]
    fn accounting_for_a_queued_batch_deselects_its_rows_and_names_the_ones_left_out() {
        let (mut dom, seen) = mount(HarnessProps {
            selected: vec![grouped_row(), serde_row(), off_page_row()],
            ..harness(|host, _| {
                let pending = PendingAction {
                    action: BulkActionKind::Merge,
                    rows: vec![grouped_row(), serde_row()],
                    retried_from: None,
                };
                let receipt = BatchReceipt {
                    left_out: vec![pr_target(&serde_row()).key()],
                };
                account_for(&pending, &receipt, host);
            })
        });

        let state = host(&dom, seen).state;
        dom.in_runtime(|| {
            assert!(!state.is_selected(&grouped_row().id), "the queued row left");
            assert!(
                !state.is_selected(&serde_row().id),
                "the left-out row left too"
            );
            assert!(
                state.is_selected(&off_page_row().id),
                "the rest of the selection stands for the next batch"
            );
        });
        let html = toasts(&mut dom);
        assert!(html.contains(r#"data-type="warning""#), "{html}");
        assert!(
            html.contains("Left out of the batch: acme/web#12 is no longer open."),
            "{html}"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// A retry the server refused the credentials of is given up whole,
    /// saying so, and the page is told it is signed out; nothing is queued,
    /// so the finished batch stays in the drawer. The retry is held while
    /// the targets are refreshed, so a second click while the first is in
    /// flight does nothing, and it is let go once the refresh is over.
    #[tokio::test]
    async fn a_retry_the_credentials_were_refused_of_is_given_up_whole_and_is_asked_for_once() {
        let (mut dom, seen) = mount(HarnessProps {
            followed: Some(followed(with_a_curable_rejection())),
            connection: Connection::SignedOut,
            ..harness(|host, seen| {
                retry_rejected(with_a_curable_rejection(), seen.retrying, host);
                retry_rejected(with_a_curable_rejection(), seen.retrying, host);
            })
        });

        assert!(
            dom.in_runtime(|| *seen.retrying.peek()),
            "the retry is held while the targets are refreshed"
        );

        drive(&mut dom).await;

        assert!(
            !dom.in_runtime(|| *seen.retrying.peek()),
            "and let go once the refresh is over"
        );
        assert_eq!(connection(&dom, seen), Connection::SignedOut);
        assert_eq!(
            following(&dom, seen).map(|followed| followed.batch_id),
            Some("batch-1".to_owned()),
            "nothing was queued, so the finished batch keeps the drawer"
        );
        let html = toasts(&mut dom);
        assert!(
            html.contains(r#"aria-label="1 notifications""#),
            "the second ask did nothing: {html}"
        );
        assert!(
            html.contains("The retry was given up: You are no longer signed in"),
            "{html}"
        );
    }

    /// Records present behaviour ahead of a reseaming.
    ///
    /// The guard a retry stands down on: whether the host is still following
    /// the batch whose targets were refreshed. The stand-down itself cannot
    /// be reached from a test today — it needs a refresh that answers with
    /// rows, which is the server — so the predicate it turns on is pinned
    /// here on its own, and the notice it leads to is not.
    #[test]
    fn a_host_follows_only_the_batch_id_it_holds() {
        let (dom, seen) = mount(HarnessProps {
            followed: Some(followed(half_done_merge())),
            ..harness(|_, _| {})
        });
        let host = host(&dom, seen);

        dom.in_runtime(|| {
            assert!(host.follows("batch-1"));
            assert!(!host.follows(BATCH));
        });
    }
}
