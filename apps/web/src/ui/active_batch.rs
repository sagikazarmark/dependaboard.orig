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
    ATTACH_TIMEOUT, BatchOutcome, Followed, SUBMIT_ATTEMPTS, ServerBatch, ServerFollow,
    follow_batch, run_batch,
};
use crate::ui::dashboard_state::{DashboardState, use_dashboard};
use crate::ui::progress_drawer::ProgressDrawer;
use crate::ui::retry::{LeftOut, ServerRefresh, left_out_notice, refresh_rejected};
use crate::ui::{PendingAction, sticky};

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
    let current = followed.read();
    let Some(current) = &*current else {
        return rsx! {};
    };
    let (dot, label) = match &current.progress {
        Some(progress) => (
            if progress.completed {
                "progress-live complete"
            } else {
                "progress-live"
            },
            format!(
                "{}: {}/{}",
                progress.action,
                progress.settled(),
                progress.targets.len()
            ),
        ),
        None => ("progress-live", "batch: following...".to_owned()),
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
                now: host.state.now(),
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
            follow: ServerFollow { batch_id },
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
/// [`queue_batch`] follows one it queued. Following the batch already
/// followed changes nothing. A batch found already finished is shown as it
/// stands and not announced: it was announced when it finished, and the page
/// has nothing to reload for it.
pub(crate) fn attach_batch(batch_id: String, mut host: BatchHost) {
    if host.follows(&batch_id) {
        return;
    }
    let followed = Followed::attaching(&batch_id, unix_seconds());
    host.take_over(followed.clone(), async move {
        let mut batch = ServerFollow { batch_id };
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
/// out of the URL.
fn conclude(outcome: BatchOutcome, mut host: BatchHost) {
    match outcome {
        BatchOutcome::Completed(progress) => {
            toast_completion(&host.toast, &progress);
            host.state.reload();
        }
        BatchOutcome::NotSubmitted(error) => {
            host.toast.error(
                format!("Batch was not submitted after {SUBMIT_ATTEMPTS} attempts: {error}"),
                sticky(),
            );
            host.followed.set(None);
        }
        BatchOutcome::Unknown => {
            let batch_id = host
                .followed
                .peek()
                .as_ref()
                .map_or_else(String::new, |followed| followed.batch_id.clone());
            host.toast.warning(
                format!(
                    "Restate has no progress for batch {batch_id} after {} seconds: it may never have run, or its workflow has been retired since. A finished batch is under Batches.",
                    ATTACH_TIMEOUT.as_secs()
                ),
                sticky(),
            );
            host.followed.set(None);
        }
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

/// Announces a finished batch. A batch always runs to the end now; what varies is whether
/// every target settled cleanly or some failed, in which case the drawer has their reasons.
fn toast_completion(toast: &Toasts, progress: &BatchProgress) {
    if progress.failed == 0 {
        toast.success("Batch complete".to_owned(), ToastOptions::new());
    } else {
        toast.warning(
            format!(
                "Batch complete: {} of {} targets failed. Open the batch for their reasons.",
                progress.failed,
                progress.targets.len()
            ),
            sticky(),
        );
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{ActionOutcome, RejectReason};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::{
        DashboardFixture, FIXTURE_NOW, followed, half_done_merge, render, serde_row,
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
        assert!(!html.contains("progress-drawer"), "{html}");
    }

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
                DashboardFixture { ActiveBatch { followed, follower, open } }
            }
        }
        let html = render(Fixture);

        assert!(html.contains(r#"class="progress-live complete""#), "{html}");
        assert!(
            html.contains("merge: 2/2"),
            "the pill counts the failed target too: {html}"
        );
        assert!(html.contains("merge progress"), "{html}");
        assert!(html.contains("1 succeeded, 0 rejected, 1 failed"), "{html}");
        assert!(
            html.contains("GitHub mutation failed with HTTP 500: Internal Server Error"),
            "the failed target's row shows its reason: {html}"
        );
    }

    /// After a reload the dashboard follows the batch its URL names by id
    /// alone: the pill is up and the drawer open on it before Restate has
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
        assert!(
            html.contains("Asking Restate where the batch stands..."),
            "{html}"
        );
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
}
