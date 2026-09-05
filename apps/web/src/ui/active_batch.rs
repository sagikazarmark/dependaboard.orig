//! The bulk action the dashboard is following: the pill that reports its
//! progress, the drawer that opens from the pill, and the way a confirmed
//! action becomes one.

use dependaboard_core::{BatchProgress, new_batch_id};
use dioxus::prelude::*;

use crate::components::toast::{ToastOptions, Toasts};
use crate::ui::batch::{BatchOutcome, STALL_TIMEOUT, SUBMIT_ATTEMPTS, ServerBatch, run_batch};
use crate::ui::progress_drawer::ProgressDrawer;
use crate::ui::{PendingAction, sticky};

/// The pill and drawer for `progress`; nothing while no batch is followed.
/// `open` says whether the drawer is showing.
#[component]
pub(crate) fn ActiveBatch(
    progress: Signal<Option<BatchProgress>>,
    mut open: Signal<bool>,
) -> Element {
    let progress = progress.read();
    let Some(progress) = &*progress else {
        return rsx! {};
    };
    rsx! {
        button {
            class: "progress-pill",
            onclick: move |_| open.toggle(),
            span { class: if progress.completed { "progress-live complete" } else { "progress-live" } }
            "{progress.action}: {progress.succeeded + progress.rejected}/{progress.targets.len()}"
        }
        if open() {
            ProgressDrawer { progress: progress.clone(), onclose: move |_| open.set(false) }
        }
    }
}

/// Queues `pending` as a bulk action and follows it to the end: `progress`
/// carries each poll's answer and the drawer opens on it, the outcome becomes
/// a toast, and a batch that ran to completion reloads the page.
pub(crate) fn queue_batch(
    PendingAction { action, targets }: PendingAction,
    mut progress: Signal<Option<BatchProgress>>,
    mut open: Signal<bool>,
    toast: Toasts,
    reload: Callback<()>,
) {
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
                match progress.failure {
                    Some(failure) => toast.error(format!("Batch failed: {failure}"), sticky()),
                    None => toast.success("Batch complete".to_owned(), ToastOptions::new()),
                }
                reload.call(());
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

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{ActionOutcome, BulkActionKind};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::{grouped_row, render, serde_row};

    /// A merge of the two fixture rows with the first one already merged.
    fn half_done_merge() -> BatchProgress {
        let targets = [pr_target(&grouped_row()), pr_target(&serde_row())];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(
            &targets[0].key(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
            },
        );
        progress
    }

    #[test]
    fn nothing_shows_while_no_batch_is_followed() {
        fn Fixture() -> Element {
            let progress = use_signal(|| None);
            let open = use_signal(|| true);
            rsx! { ActiveBatch { progress, open } }
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
            rsx! { ActiveBatch { progress, open } }
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
                progress.fail("Restate went away");
                Some(progress)
            });
            let open = use_signal(|| true);
            rsx! { ActiveBatch { progress, open } }
        }
        let html = render(Fixture);

        assert!(html.contains(r#"class="progress-live complete""#), "{html}");
        assert!(html.contains("merge progress"), "{html}");
        assert!(html.contains("1 succeeded, 0 rejected"), "{html}");
        assert!(html.contains("Restate went away"), "{html}");
    }
}
