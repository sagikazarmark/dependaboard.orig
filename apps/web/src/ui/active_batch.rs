//! The bulk action the dashboard is following: the pill that reports its
//! progress, the drawer that opens from the pill, the way a confirmed action
//! becomes one, and the way a finished one's rejected targets become another.

use dependaboard_core::{BatchProgress, new_batch_id};
use dioxus::prelude::*;

use crate::components::toast::{ToastOptions, Toasts, use_toast};
use crate::ui::batch::{BatchOutcome, STALL_TIMEOUT, SUBMIT_ATTEMPTS, ServerBatch, run_batch};
use crate::ui::dashboard_state::{DashboardState, use_dashboard};
use crate::ui::progress_drawer::ProgressDrawer;
use crate::ui::retry::{ServerRefresh, refresh_rejected};
use crate::ui::{PendingAction, sticky};

/// Where a queued batch lands: the progress the pill and drawer read, whether
/// the drawer is showing, the toasts its outcome goes to, and the dashboard it
/// reloads once it has run.
#[derive(Clone, Copy)]
pub(crate) struct BatchHost {
    pub(crate) progress: Signal<Option<BatchProgress>>,
    pub(crate) open: Signal<bool>,
    pub(crate) toast: Toasts,
    pub(crate) state: DashboardState,
}

/// The pill and drawer for `progress`; nothing while no batch is followed.
/// `open` says whether the drawer is showing.
///
/// The drawer's **Retry rejected** is handled here: the finished batch's
/// rejected targets are refreshed and queued as a new batch of the same
/// kind, which takes the drawer over as any queued batch does.
#[component]
pub(crate) fn ActiveBatch(
    progress: Signal<Option<BatchProgress>>,
    mut open: Signal<bool>,
) -> Element {
    let host = BatchHost {
        progress,
        open,
        toast: use_toast(),
        state: use_dashboard(),
    };
    let retrying = use_signal(|| false);
    let followed = progress.read();
    let Some(followed) = &*followed else {
        return rsx! {};
    };
    rsx! {
        button {
            class: "progress-pill",
            onclick: move |_| open.toggle(),
            span { class: if followed.completed { "progress-live complete" } else { "progress-live" } }
            "{followed.action}: {followed.settled()}/{followed.targets.len()}"
        }
        if open() {
            ProgressDrawer {
                progress: followed.clone(),
                retrying: retrying(),
                onretry: move |_| {
                    if let Some(finished) = progress.peek().clone() {
                        retry_rejected(finished, retrying, host);
                    }
                },
                onclose: move |_| open.set(false),
            }
        }
    }
}

/// Queues `pending` as a bulk action and follows it to the end: the host's
/// progress carries each poll's answer and its drawer opens on it, the
/// outcome becomes a toast, and a batch that ran to completion reloads the
/// page.
pub(crate) fn queue_batch(pending: PendingAction, host: BatchHost) {
    let BatchHost {
        mut progress,
        mut open,
        toast,
        mut state,
    } = host;
    let action = pending.action;
    let targets = pending.targets();
    let batch_id = new_batch_id();
    progress.set(Some(BatchProgress::queued(&batch_id, action, &targets)));
    open.set(true);
    spawn(async move {
        let mut batch = ServerBatch {
            batch_id,
            action,
            targets,
        };
        let outcome = run_batch(&mut batch, |update| {
            progress.set(Some(update.clone()));
        })
        .await;
        match outcome {
            BatchOutcome::Completed(progress) => {
                toast_completion(&toast, &progress);
                state.reload();
            }
            BatchOutcome::NotSubmitted(error) => toast.error(
                format!("Batch was not submitted after {SUBMIT_ATTEMPTS} attempts: {error}"),
                sticky(),
            ),
            BatchOutcome::Stalled(error) => {
                let reason = error.unwrap_or_else(|| {
                    format!("no progress for {} minutes", STALL_TIMEOUT.as_secs() / 60)
                });
                toast.error(
                    format!(
                        "Lost track of the batch ({reason}). It may still be running in Restate; reload to see where it got to."
                    ),
                    sticky(),
                );
            }
        }
    });
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
        let still_followed = host
            .progress
            .peek()
            .as_ref()
            .is_some_and(|followed| followed.batch_id == finished.batch_id);
        if !still_followed {
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
    use crate::ui::test_support::{DashboardFixture, half_done_merge, render, serde_row};

    #[test]
    fn nothing_shows_while_no_batch_is_followed() {
        fn Fixture() -> Element {
            let progress = use_signal(|| None);
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { ActiveBatch { progress, open } }
            }
        }
        let html = render(Fixture);

        assert!(!html.contains("progress-pill"), "{html}");
        assert!(!html.contains("progress-drawer"), "{html}");
    }

    #[test]
    fn the_pill_counts_the_batch_and_keeps_the_drawer_closed_until_asked() {
        fn Fixture() -> Element {
            let progress = use_signal(|| Some(half_done_merge()));
            let open = use_signal(|| false);
            rsx! {
                DashboardFixture { ActiveBatch { progress, open } }
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
            let progress = use_signal(|| {
                let mut progress = half_done_merge();
                progress.record_failure(
                    &pr_target(&serde_row()).key(),
                    "GitHub mutation failed with HTTP 500: Internal Server Error",
                );
                Some(progress)
            });
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { ActiveBatch { progress, open } }
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

    /// The retry needs the toasts and the dashboard around the drawer, which
    /// is what this component gives it; so it is here that the drawer's offer
    /// is mounted with them in place.
    #[test]
    fn an_open_drawer_on_a_finished_batch_with_a_rejection_offers_the_retry() {
        fn Fixture() -> Element {
            let progress = use_signal(|| {
                let mut progress = half_done_merge();
                progress.record(
                    &pr_target(&serde_row()).key(),
                    ActionOutcome::Rejected {
                        reason: RejectReason::Forbidden,
                    },
                );
                Some(progress)
            });
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { ActiveBatch { progress, open } }
            }
        }
        let html = render(Fixture);

        assert!(html.contains(">Retry rejected<"), "{html}");
    }
}
