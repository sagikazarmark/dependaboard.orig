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
use crate::ui::{Fault, PendingAction, sticky};

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
/// kind, which replaces `finished` in the host and so in the drawer.
/// `retrying` is held while the targets are being refreshed.
///
/// The refresh can take a while, and the user may confirm another batch in
/// the meantime; a retry that finds the host following a batch other than
/// `finished` stands down rather than take the drawer from the one running.
fn retry_rejected(finished: BatchProgress, mut retrying: Signal<bool>, host: BatchHost) {
    if retrying() {
        return;
    }
    retrying.set(true);
    spawn(async move {
        let refreshed = refresh_rejected(&ServerRefresh, &finished).await;
        retrying.set(false);
        if let Some(notice) = refreshed.notice() {
            host.toast.warning(notice, sticky());
        }
        if refreshed.rows.is_empty() {
            return;
        }
        if !host.follows(&finished.batch_id) {
            host.toast.info(
                "The retry stood down: another batch was queued while its targets were being refreshed."
                    .to_owned(),
                sticky(),
            );
            return;
        }
        queue_batch(
            PendingAction {
                action: finished.action,
                rows: refreshed.rows,
            },
            host,
        );
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
    use dependaboard_core::{ActionOutcome, BulkActionKind, RejectReason};

    use super::*;
    use crate::ui::batch::WAITING_NOTICE_AFTER;
    use crate::ui::pr_target;
    use crate::ui::test_support::{
        DashboardFixture, FIXTURE_NOW, followed, grouped_row, half_done_merge, render, serde_row,
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
}
