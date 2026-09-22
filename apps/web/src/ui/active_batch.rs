//! The bulk action the dashboard is following: the pill that reports its
//! progress, the drawer that opens from the pill, the ways a batch becomes
//! the followed one — confirmed here, named in the URL, picked from the audit
//! view, or a finished one's rejected targets queued again — and what is said
//! when the follow ends.
//!
//! One batch is followed at a time. Following another, however it comes, ends
//! the follow before it, so two never report into the one pill.

use dependaboard_core::{BatchProgress, BatchReceipt, PrRecord, new_batch_id, unix_seconds};
use dioxus::core::Task;
use dioxus::prelude::*;

use crate::components::toast::{ToastOptions, Toasts, use_toast};
use crate::ui::batch::{
    ATTACH_TIMEOUT, AttachGateway, BatchOutcome, Followed, Listing, SUBMIT_ATTEMPTS, ServerBatch,
    ServerFollow, follow_batch, run_batch,
};
use crate::ui::dashboard_state::{Connection, DashboardState, use_dashboard};
use crate::ui::format::{relative_time, verdict_tally};
use crate::ui::progress_drawer::ProgressDrawer;
use crate::ui::retry::{
    LeftOut, Refreshed, ServerRefresh, SignedOut, left_out_notice, refresh_rejected,
};
use crate::ui::{Fault, PendingAction, SIGNED_OUT_MESSAGE, sticky};

/// Something the followed batch has to say, and how it is meant to be read.
/// Only a clean completion goes away on its own; every other notice carries
/// something the user has to act on or go and look at, so it stays until it
/// is dismissed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    /// A batch that ran to the end with nothing to report.
    Success(String),
    /// Something the batch did not do, said without blame: nothing failed.
    Info(String),
    /// Something that did not go as asked, which the user has to read.
    Warning(String),
    /// Something that did not happen at all.
    Error(String),
}

/// What following a batch does to the page around it: the notices it leaves,
/// what it makes of the follow itself, the batch it queues in place of one
/// that finished, and the dashboard state it asks to change.
///
/// This is the page, not the server — the server is [`SubmitGateway`] and its
/// kin, which the follow reads through and which answer in their own time.
/// Nothing here waits on anything, so a whole lifecycle decision can be run
/// against a recorder without a runtime under it.
///
/// [`SubmitGateway`]: crate::ui::batch::SubmitGateway
pub(crate) trait BatchEffects {
    /// Says `notice` to the user.
    fn announce(&mut self, notice: Notice);

    /// The batch being followed, as far as the page has been told.
    fn following(&self) -> Option<Followed>;

    /// Publishes `followed` as what is now known of the batch being followed.
    fn follow(&mut self, followed: Followed);

    /// Ends the follow: no batch is being followed, which takes the pill and
    /// the drawer down and the batch out of the URL.
    fn stand_down(&mut self);

    /// Queues `pending` as a new batch and follows it, taking the drawer over
    /// as any queued batch does.
    fn queue(&mut self, pending: PendingAction);

    /// Takes `rows` out of the selection.
    fn deselect(&mut self, rows: &[PrRecord]);

    /// Reloads the dashboard, so it shows what the batch changed.
    fn reload(&mut self);

    /// Tells the page the server has refused its credentials, so the banner
    /// goes up at once and the live refresh asks nothing more.
    fn signed_out(&mut self);

    /// Whether the batch `batch_id` names is the one being followed.
    fn follows(&self, batch_id: &str) -> bool {
        self.following()
            .is_some_and(|followed| followed.batch_id == batch_id)
    }
}

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
}

/// The page as the dashboard's own components make it: notices are toasts,
/// the follow is the host's signals, and everything else is the dashboard's
/// state. A notice stays up by its kind — a clean completion is the one that
/// goes away on its own, as it always did.
impl BatchEffects for BatchHost {
    fn announce(&mut self, notice: Notice) {
        match notice {
            Notice::Success(said) => self.toast.success(said, ToastOptions::new()),
            Notice::Info(said) => self.toast.info(said, sticky()),
            Notice::Warning(said) => self.toast.warning(said, sticky()),
            Notice::Error(said) => self.toast.error(said, sticky()),
        }
    }

    fn following(&self) -> Option<Followed> {
        self.followed.peek().clone()
    }

    fn follow(&mut self, followed: Followed) {
        self.followed.set(Some(followed));
    }

    fn stand_down(&mut self) {
        self.followed.set(None);
    }

    fn queue(&mut self, pending: PendingAction) {
        queue_batch(pending, *self);
    }

    fn deselect(&mut self, rows: &[PrRecord]) {
        self.state.deselect(rows);
    }

    fn reload(&mut self) {
        self.state.reload();
    }

    fn signed_out(&mut self) {
        self.state.poll_missed(Connection::SignedOut);
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
        let mut reporting = host;
        let mut accounting = host;
        let outcome = run_batch(
            &mut batch,
            followed,
            |update| reporting.follow(update.clone()),
            |receipt| account_for(&pending, &receipt, &mut accounting),
        )
        .await;
        conclude(outcome, &mut host);
    });
}

/// Accounts for `pending` once Restate has the batch it was queued as. Its
/// rows leave the selection — the ones queued and the ones left out alike,
/// since neither is for a next batch — and the targets the server left out,
/// pull requests the projection no longer had by the time the batch was
/// submitted, are named with their repositories, as the retry names the
/// targets it leaves out.
fn account_for(pending: &PendingAction, receipt: &BatchReceipt, effects: &mut impl BatchEffects) {
    effects.deselect(&pending.rows);
    let left_out: Vec<LeftOut> = pending
        .targets()
        .iter()
        .filter(|target| receipt.left_out.contains(&target.key()))
        .map(LeftOut::no_longer_open)
        .collect();
    if let Some(notice) = left_out_notice("the batch", &left_out) {
        effects.announce(Notice::Warning(notice));
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
        follow_to_end(&mut batch, followed, &mut host).await;
    });
}

/// Follows `followed` through `gateway` to the end and says how it ended,
/// except for the one ending that is not news: a batch that was already over
/// when the follow began — never once seen running — completes the moment it
/// is looked up, and was announced when it finished. It is shown as it
/// stands, with nothing said and nothing reloaded; the page never had it
/// running, so it has nothing of it to bring up to date.
async fn follow_to_end<G: AttachGateway>(
    gateway: &mut G,
    followed: Followed,
    effects: &mut impl BatchEffects,
) {
    let mut seen_running = false;
    let outcome = follow_batch(gateway, followed, |update| {
        seen_running |= update.heard && !update.completed();
        effects.follow(update.clone());
    })
    .await;
    if !seen_running && matches!(outcome, BatchOutcome::Completed(_)) {
        return;
    }
    conclude(outcome, effects);
}

/// Says how the follow ended. A batch that ran to the end is announced and
/// the page reloaded. One Restate would not take, or has no progress for, is
/// no batch to follow: the pill and drawer stand down, which also takes it
/// out of the URL. A follow the server refused the credentials of stands as
/// it was — the drawer says why, and the batch stays in the URL for the
/// reload to pick up — and the page is told it is signed out, as it is when
/// the submission itself was refused, so the banner goes up at once and the
/// live refresh asks nothing more either.
fn conclude(outcome: BatchOutcome, effects: &mut impl BatchEffects) {
    match outcome {
        BatchOutcome::Completed(progress) => {
            effects.announce(match completion_notice(&progress) {
                None => Notice::Success("Batch complete".to_owned()),
                Some(notice) => Notice::Warning(notice),
            });
            effects.reload();
        }
        BatchOutcome::NotSubmitted(fault) => {
            effects.announce(Notice::Error(not_submitted_notice(&fault)));
            effects.stand_down();
            if fault == Fault::SignedOut {
                effects.signed_out();
            }
        }
        BatchOutcome::Unknown => {
            let notice = effects
                .following()
                .as_ref()
                .map_or_else(String::new, unknown_notice);
            effects.announce(Notice::Warning(notice));
            effects.stand_down();
        }
        BatchOutcome::SignedOut => effects.signed_out(),
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
/// targets are being refreshed, so a second ask while the first is in flight
/// does nothing; what becomes of the refreshed targets is [`queue_retry`]'s.
fn retry_rejected(finished: BatchProgress, mut retrying: Signal<bool>, mut host: BatchHost) {
    if retrying() {
        return;
    }
    retrying.set(true);
    spawn(async move {
        let refreshed = refresh_rejected(&ServerRefresh { state: host.state }, &finished).await;
        retrying.set(false);
        queue_retry(&finished.batch_id, refreshed, &mut host);
    });
}

/// What becomes of the retry of the batch `batch_id` names once its rejected
/// targets have been refreshed: the ones there is no retrying are named, and
/// the rest are queued as a new batch that takes the drawer over.
///
/// The refresh can take a while, and the user may confirm another batch in
/// the meantime; a retry that comes back to find another batch followed
/// stands down and says so, rather than take the drawer from the one
/// running. A retry the server refused the credentials of is given up whole,
/// saying so, and the page is told it is signed out.
fn queue_retry(
    batch_id: &str,
    refreshed: Result<Refreshed, SignedOut>,
    effects: &mut impl BatchEffects,
) {
    let Ok(refreshed) = refreshed else {
        effects.announce(Notice::Error(format!(
            "The retry was given up: {SIGNED_OUT_MESSAGE}"
        )));
        effects.signed_out();
        return;
    };
    if let Some(notice) = refreshed.notice() {
        effects.announce(Notice::Warning(notice));
    }
    let Some(retry) = refreshed.retry() else {
        return;
    };
    if !effects.follows(batch_id) {
        effects.announce(Notice::Info(
            "The retry stood down: another batch was queued while its targets were being refreshed."
                .to_owned(),
        ));
        return;
    }
    effects.queue(retry);
}

/// What the completion notice says past a clean "Batch complete", if anything:
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
    use std::collections::VecDeque;
    use std::time::Duration;

    use dependaboard_core::{ActionOutcome, BulkActionKind, ProjectedBatch, RejectReason};
    use dioxus::core::consume_context_from_scope;

    use super::*;
    use crate::ui::batch::{BatchGateway, WAITING_NOTICE_AFTER};
    use crate::ui::pr_target;
    use crate::ui::test_support::{
        BATCH, DashboardFixture, FIXTURE_NOW, followed, grouped_row, half_done_merge,
        next_or_repeat, render, serde_row,
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

    // The lifecycle divides into decisions and wiring. A decision — what to
    // say, what to queue, what to stand down — is a plain function over
    // [`BatchEffects`] and is run below against [`RecordedPage`], with no
    // runtime under it and the recorder read for what it did. The wiring —
    // [`BatchHost::take_over`]'s cancel-and-open, the `spawn`s, the hold on a
    // second retry — only exists inside a live scope, so those tests mount
    // the harness and read the host's own signals.

    /// Stands in for the page a followed batch reports into, and records what
    /// it did there rather than doing it: `notices` is everything announced,
    /// in order; `followed` is the batch the page follows, as the decision
    /// left it, and `stood_down` whether it was taken down; `queued` is every
    /// batch queued; `deselected` is every row taken out of the selection;
    /// `reloads` counts the dashboard reloads and `signed_out` says whether
    /// the page was told the server refused it.
    #[derive(Debug, Default, PartialEq)]
    struct RecordedPage {
        notices: Vec<Notice>,
        followed: Option<Followed>,
        stood_down: bool,
        queued: Vec<PendingAction>,
        deselected: Vec<PrRecord>,
        reloads: u32,
        signed_out: bool,
    }

    impl BatchEffects for RecordedPage {
        fn announce(&mut self, notice: Notice) {
            self.notices.push(notice);
        }

        fn following(&self) -> Option<Followed> {
            self.followed.clone()
        }

        fn follow(&mut self, followed: Followed) {
            self.followed = Some(followed);
        }

        fn stand_down(&mut self) {
            self.followed = None;
            self.stood_down = true;
        }

        fn queue(&mut self, pending: PendingAction) {
            self.queued.push(pending);
        }

        fn deselect(&mut self, rows: &[PrRecord]) {
            self.deselected.extend_from_slice(rows);
        }

        fn reload(&mut self) {
            self.reloads += 1;
        }

        fn signed_out(&mut self) {
            self.signed_out = true;
        }
    }

    /// A page following the batch `progress` is of, and nothing else done to
    /// it yet.
    fn page(progress: BatchProgress) -> RecordedPage {
        RecordedPage {
            followed: Some(followed(progress)),
            ..RecordedPage::default()
        }
    }

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

    /// A batch that ran to the end is announced with its tally and the page
    /// reloaded, and the follow is left standing: the drawer stays open on
    /// the finished batch, and the URL keeps it.
    #[test]
    fn a_batch_that_ran_to_the_end_is_announced_and_reloads_the_page_with_the_follow_left_standing()
    {
        let mut page = page(half_done_merge());

        conclude(BatchOutcome::Completed(one_of_each()), &mut page);

        assert_eq!(
            page.notices,
            vec![Notice::Warning(
                "Batch complete: 1 succeeded, 1 rejected, 1 failed. Open the batch for their reasons."
                    .to_owned()
            )]
        );
        assert_eq!(page.reloads, 1);
        assert!(page.following().is_some(), "the follow stands");
        assert!(!page.signed_out);
    }

    /// A batch Restate would not take is no batch to follow: the pill and
    /// drawer stand down, which takes it out of the URL too, and the page is
    /// not reloaded — nothing ran. The line to the server is left as it was:
    /// Restate not answering is not the browser being signed out.
    #[test]
    fn a_batch_restate_would_not_take_stands_the_follow_down_without_a_reload_or_a_sign_out() {
        let mut page = page(half_done_merge());

        conclude(
            BatchOutcome::NotSubmitted(Fault::Refused("Restate is unavailable".to_owned())),
            &mut page,
        );

        assert_eq!(
            page.notices,
            vec![Notice::Error(
                "Batch was not submitted after 5 attempts: Restate is unavailable".to_owned()
            )]
        );
        assert!(page.stood_down, "the follow stood down");
        assert_eq!(page.following(), None);
        assert_eq!(page.reloads, 0);
        assert!(!page.signed_out);
    }

    /// The submission being refused the credentials is the one fault that
    /// both stands the follow down — there is no batch to pick up on the
    /// reload, so nothing should stay in the URL — and signs the page out, so
    /// the banner goes up at once and the live refresh asks nothing more. It
    /// is the pair of the refused *follow* below, which does the second and
    /// not the first.
    #[test]
    fn a_submission_the_credentials_were_refused_of_stands_the_follow_down_and_signs_the_page_out()
    {
        let mut page = page(half_done_merge());

        conclude(BatchOutcome::NotSubmitted(Fault::SignedOut), &mut page);

        assert_eq!(
            page.notices,
            vec![Notice::Error(
                "Batch was not submitted: You are no longer signed in — reload to sign in again"
                    .to_owned()
            )]
        );
        assert!(page.stood_down, "the follow stood down");
        assert!(page.signed_out);
        assert_eq!(page.reloads, 0);
    }

    /// A follow the server refused the credentials of stands as it was: the
    /// batch is still running in Restate, so it stays followed and stays in
    /// the URL for the reload to pick up. The page is signed out all the
    /// same, and nothing is announced — the drawer says it.
    #[test]
    fn a_follow_the_credentials_were_refused_of_signs_the_page_out_and_leaves_the_follow_standing()
    {
        let mut page = page(half_done_merge());

        conclude(BatchOutcome::SignedOut, &mut page);

        assert_eq!(page.notices, Vec::new(), "nothing is said in a notice");
        assert!(
            page.following().is_some(),
            "the batch stays followed, so the URL keeps it across the reload"
        );
        assert!(!page.stood_down);
        assert!(page.signed_out);
        assert_eq!(page.reloads, 0);
    }

    /// A batch nobody would vouch for stands the follow down with the
    /// projection's word on it in the notice, read off the follow as it
    /// ended — before it is taken down. The page is neither reloaded nor
    /// signed out.
    #[test]
    fn a_batch_nobody_would_vouch_for_stands_the_follow_down_and_says_which_link_it_was() {
        let mut page = RecordedPage {
            followed: Some(Followed {
                listing: Listing::Unlisted,
                ..followed(half_done_merge())
            }),
            ..RecordedPage::default()
        };

        conclude(BatchOutcome::Unknown, &mut page);

        let [Notice::Warning(said)] = &page.notices[..] else {
            panic!("one warning is announced, not {:?}", page.notices);
        };
        assert!(
            said.starts_with("No batch batch-1: the projection has no record of it"),
            "the notice is read off the follow before it is taken down: {said}"
        );
        assert!(page.stood_down, "the follow stood down");
        assert_eq!(page.reloads, 0);
        assert!(!page.signed_out);
    }

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

    /// Once Restate has the batch, every row the action was queued with
    /// leaves the selection — the ones it runs and the ones the server left
    /// out alike, since neither is for a next batch — and the left-out ones
    /// are named with their repositories, in the retry's words. Those rows
    /// and no others: deselecting is all that is done to the selection, so
    /// the rest of it stands for the next batch.
    #[test]
    fn accounting_for_a_queued_batch_deselects_its_rows_and_names_the_ones_left_out() {
        let mut page = RecordedPage::default();
        let pending = PendingAction {
            action: BulkActionKind::Merge,
            rows: vec![grouped_row(), serde_row()],
            retried_from: None,
        };
        let receipt = BatchReceipt {
            left_out: vec![pr_target(&serde_row()).key()],
        };

        account_for(&pending, &receipt, &mut page);

        assert_eq!(
            page.deselected,
            vec![grouped_row(), serde_row()],
            "the queued row and the left-out one both leave, and nothing else does"
        );
        assert_eq!(
            page.notices,
            vec![Notice::Warning(
                "Left out of the batch: acme/web#12 is no longer open.".to_owned()
            )]
        );
        assert!(page.queued.is_empty());
    }

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

    /// The guard a retry stands down on: whether the host is still following
    /// the batch whose targets were refreshed. The host reads it off its own
    /// signal, which is what the recorder stands in for everywhere else.
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

    /// The refresh of a batch's rejected targets, come back with `rows` to
    /// queue again and nothing left out.
    fn refreshed(batch_id: &str, rows: Vec<PrRecord>) -> Refreshed {
        Refreshed {
            batch_id: batch_id.to_owned(),
            action: BulkActionKind::Merge,
            rows,
            left_out: Vec::new(),
        }
    }

    /// Refreshing a retry's targets takes as long as it takes, and the user
    /// may confirm another batch in the meantime. The batch running has the
    /// drawer, and a retry that comes back to find it there stands down: it
    /// does not queue, and it does not take the drawer from a batch the user
    /// is watching. It says so, once, and without blame — nothing failed,
    /// and the finished batch is still there to retry again.
    #[test]
    fn a_retry_whose_batch_lost_the_drawer_stands_down_rather_than_take_it_from_the_one_running() {
        let mut page = page(half_done_merge());

        queue_retry(BATCH, Ok(refreshed(BATCH, vec![serde_row()])), &mut page);

        assert_eq!(
            page.following().map(|followed| followed.batch_id),
            Some("batch-1".to_owned()),
            "the batch queued in the meantime keeps the drawer"
        );
        assert_eq!(page.queued, Vec::new(), "nothing was queued");
        assert_eq!(
            page.notices,
            vec![Notice::Info(
                "The retry stood down: another batch was queued while its targets were being refreshed."
                    .to_owned()
            )]
        );
    }

    /// The same refresh, come back to find its own batch still followed, is
    /// queued as the retry it was asked for: the pair of the stand-down, so
    /// the guard is read as a guard and not as a refusal to retry at all.
    #[test]
    fn a_retry_whose_batch_still_has_the_drawer_queues_the_refreshed_rows() {
        let mut page = page(half_done_merge());

        queue_retry(
            "batch-1",
            Ok(refreshed("batch-1", vec![serde_row()])),
            &mut page,
        );

        assert_eq!(
            page.queued,
            vec![PendingAction {
                action: BulkActionKind::Merge,
                rows: vec![serde_row()],
                retried_from: Some("batch-1".to_owned()),
            }]
        );
        assert_eq!(page.notices, Vec::new(), "nothing was left out to say");
    }

    /// A server for a batch followed by id alone, scripted: what the
    /// projection says of it, then Restate's answers in order, the last
    /// repeating. The clock stands still and a tick costs nothing, so a
    /// follow runs to its end as fast as it is asked.
    struct ScriptedFollow {
        projected: Result<Option<ProjectedBatch>, Fault>,
        progress: VecDeque<Result<Option<BatchProgress>, Fault>>,
    }

    impl ScriptedFollow {
        /// A projection that has never heard of the batch, with `progress` as
        /// Restate's answers.
        fn unlisted(progress: impl IntoIterator<Item = BatchProgress>) -> Self {
            Self {
                projected: Ok(None),
                progress: progress.into_iter().map(|it| Ok(Some(it))).collect(),
            }
        }
    }

    impl BatchGateway for ScriptedFollow {
        async fn progress(&mut self) -> Result<Option<BatchProgress>, Fault> {
            next_or_repeat(&mut self.progress)
        }

        async fn tick(&mut self) {}

        fn now(&self) -> u64 {
            FIXTURE_NOW
        }
    }

    impl AttachGateway for ScriptedFollow {
        async fn projected(&mut self) -> Result<Option<ProjectedBatch>, Fault> {
            self.projected.clone()
        }
    }

    /// A batch followed by id alone that was already over by the time the
    /// follow reached it — Restate's first answer is the finished progress,
    /// and it was never once seen running — is shown as it stands and left
    /// at that. It was announced when it finished, to whoever was watching
    /// it then, and the page has nothing of it to reload for.
    #[tokio::test]
    async fn a_batch_that_was_over_before_the_follow_began_is_shown_and_not_announced_again() {
        let done = finished(ActionOutcome::Succeeded {
            detail: "merged".to_owned(),
            merge_sha: None,
        });
        let mut gateway = ScriptedFollow::unlisted([done.clone()]);
        let mut page = RecordedPage::default();

        follow_to_end(
            &mut gateway,
            Followed::attaching("batch-1", FIXTURE_NOW),
            &mut page,
        )
        .await;

        assert_eq!(page.notices, Vec::new(), "it was announced when it ran");
        assert_eq!(page.reloads, 0, "there is nothing of it to reload for");
        assert_eq!(
            page.following().and_then(|followed| followed.progress),
            Some(done),
            "the drawer shows the batch as it stands"
        );
        assert!(!page.stood_down, "the follow stands on the finished batch");
    }

    /// The same follow over a batch that was still running when it reached
    /// it: that one the page did watch, so its completion is news, and it is
    /// announced and the dashboard reloaded as any batch followed to the end
    /// is. The pair of the test above, so the suppression is read as being
    /// about a batch that was already over and not about finishing at all.
    #[tokio::test]
    async fn a_batch_seen_running_is_announced_and_reloads_the_page_when_it_completes() {
        let done = finished(ActionOutcome::Succeeded {
            detail: "merged".to_owned(),
            merge_sha: None,
        });
        let mut gateway = ScriptedFollow::unlisted([half_done_merge(), done]);
        let mut page = RecordedPage::default();

        follow_to_end(
            &mut gateway,
            Followed::attaching("batch-1", FIXTURE_NOW),
            &mut page,
        )
        .await;

        assert_eq!(
            page.notices,
            vec![Notice::Success("Batch complete".to_owned())]
        );
        assert_eq!(page.reloads, 1);
        assert!(!page.stood_down, "the follow stands on the finished batch");
    }
}
