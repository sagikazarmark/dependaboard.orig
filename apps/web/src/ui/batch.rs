//! The bulk-action flow after confirmation: submit the batch to Restate, then
//! follow its progress until it completes or stops moving.

use std::time::Duration;

use dependaboard_core::{BatchProgress, BulkActionKind, PrTarget};

use crate::api::{load_batch_progress, submit_batch};
use crate::ui::{POLL_INTERVAL, sleep, user_facing};

/// How many times a submission is tried, one poll interval apart, before the
/// batch is reported as not submitted.
pub(crate) const SUBMIT_ATTEMPTS: u32 = 5;

/// How long the progress may go without changing before the dashboard stops
/// following the batch. Long enough for a target to sit out GitHub's
/// rate-limit waits (up to three of sixty seconds) without the batch being
/// declared lost.
pub(crate) const STALL_TIMEOUT: Duration = Duration::from_secs(4 * 60);

/// [`STALL_TIMEOUT`] in polls.
const STALL_LIMIT: u32 = (STALL_TIMEOUT.as_secs() / POLL_INTERVAL.as_secs()) as u32;

/// The server as the flow sees it, so the flow can be driven by a script in
/// tests. Errors are already in their user-facing form.
pub(crate) trait BatchGateway {
    async fn submit(&mut self) -> Result<(), String>;
    async fn progress(&mut self) -> Result<Option<BatchProgress>, String>;
    /// Waits one poll interval.
    async fn tick(&mut self);
}

/// One batch, driven through the server functions.
pub(crate) struct ServerBatch {
    pub(crate) batch_id: String,
    pub(crate) action: BulkActionKind,
    pub(crate) targets: Vec<PrTarget>,
}

impl BatchGateway for ServerBatch {
    async fn submit(&mut self) -> Result<(), String> {
        submit_batch(self.batch_id.clone(), self.action, self.targets.clone())
            .await
            .map_err(|error| user_facing(&error))
    }

    async fn progress(&mut self) -> Result<Option<BatchProgress>, String> {
        load_batch_progress(self.batch_id.clone())
            .await
            .map_err(|error| user_facing(&error))
    }

    async fn tick(&mut self) {
        sleep(POLL_INTERVAL).await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BatchOutcome {
    Completed(BatchProgress),
    /// Restate never accepted the batch; carries the last submission error.
    NotSubmitted(String),
    /// The progress did not change for [`STALL_LIMIT`] polls. The batch may
    /// well still be running in Restate. Carries the last poll error, if the
    /// polls themselves were failing.
    Stalled(Option<String>),
}

/// Submits the batch and follows it, calling `report` with each new progress
/// snapshot, until it completes, cannot be submitted, or stops moving.
pub(crate) async fn run_batch<G: BatchGateway>(
    gateway: &mut G,
    mut report: impl FnMut(&BatchProgress),
) -> BatchOutcome {
    let mut last = match submit(gateway).await {
        Ok(progress) => progress,
        Err(error) => return BatchOutcome::NotSubmitted(error),
    };
    if let Some(progress) = &last {
        report(progress);
        if progress.completed {
            return BatchOutcome::Completed(progress.clone());
        }
    }
    let mut stalled = 0;
    let mut last_error = None;
    loop {
        gateway.tick().await;
        match gateway.progress().await {
            Ok(Some(progress)) if last.as_ref() != Some(&progress) => {
                report(&progress);
                if progress.completed {
                    return BatchOutcome::Completed(progress);
                }
                last = Some(progress);
                stalled = 0;
                last_error = None;
            }
            Ok(_) => stalled += 1,
            Err(error) => {
                stalled += 1;
                last_error = Some(error);
            }
        }
        if stalled == STALL_LIMIT {
            return BatchOutcome::Stalled(last_error);
        }
    }
}

/// Gets the batch accepted. `Ok(None)` is a clean acceptance; `Ok(Some)` means
/// a round trip failed after Restate had already taken the batch, and carries
/// the progress that proved it.
async fn submit<G: BatchGateway>(gateway: &mut G) -> Result<Option<BatchProgress>, String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match gateway.submit().await {
            Ok(()) => return Ok(None),
            Err(error) => error,
        };
        // A failed round trip does not mean Restate did not take the batch;
        // the workflow's own progress is the authority, and a batch that is
        // running must not be submitted again.
        if let Ok(Some(progress)) = gateway.progress().await {
            return Ok(Some(progress));
        }
        if attempt == SUBMIT_ATTEMPTS {
            return Err(error);
        }
        gateway.tick().await;
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::collections::VecDeque;

    use dependaboard_core::{ActionOutcome, BulkActionKind, PrTarget};

    use super::*;
    use crate::ui::pr_target;
    use crate::ui::test_support::grouped_row;

    /// A server whose answers are scripted: each call pops the next answer,
    /// and the last one repeats once the script runs out.
    struct Scripted {
        submits: VecDeque<Result<(), String>>,
        progress: VecDeque<Result<Option<BatchProgress>, String>>,
        submit_calls: u32,
        ticks: u32,
    }

    impl Scripted {
        fn new(
            submits: impl IntoIterator<Item = Result<(), String>>,
            progress: impl IntoIterator<Item = Result<Option<BatchProgress>, String>>,
        ) -> Self {
            Self {
                submits: submits.into_iter().collect(),
                progress: progress.into_iter().collect(),
                submit_calls: 0,
                ticks: 0,
            }
        }
    }

    fn next_or_repeat<T: Clone>(script: &mut VecDeque<T>) -> T {
        if script.len() > 1 {
            script.pop_front().unwrap()
        } else {
            script.front().cloned().expect("the script is not empty")
        }
    }

    impl BatchGateway for Scripted {
        async fn submit(&mut self) -> Result<(), String> {
            self.submit_calls += 1;
            next_or_repeat(&mut self.submits)
        }

        async fn progress(&mut self) -> Result<Option<BatchProgress>, String> {
            next_or_repeat(&mut self.progress)
        }

        async fn tick(&mut self) {
            self.ticks += 1;
        }
    }

    fn unavailable() -> String {
        "Restate is unavailable".to_owned()
    }

    fn targets() -> Vec<PrTarget> {
        let mut second = grouped_row();
        second.number = 10;
        vec![pr_target(&grouped_row()), pr_target(&second)]
    }

    fn queued() -> BatchProgress {
        BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets())
    }

    fn after(steps: usize) -> BatchProgress {
        let mut progress = queued();
        for target in targets().iter().take(steps) {
            progress.record(
                &target.key(),
                ActionOutcome::Succeeded {
                    detail: "merged".to_owned(),
                },
            );
        }
        progress
    }

    fn repeat<T: Clone>(value: T, times: u32) -> impl Iterator<Item = T> {
        std::iter::repeat_n(value, times as usize)
    }

    async fn run(gateway: &mut Scripted) -> (BatchOutcome, Vec<BatchProgress>) {
        let mut reported = Vec::new();
        let outcome = run_batch(gateway, |progress| reported.push(progress.clone())).await;
        (outcome, reported)
    }

    #[tokio::test]
    async fn a_submission_that_keeps_failing_is_given_up_after_the_attempt_budget() {
        let mut gateway = Scripted::new([Err(unavailable())], [Ok(None)]);

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::NotSubmitted(unavailable()));
        assert_eq!(gateway.submit_calls, SUBMIT_ATTEMPTS);
        assert!(reported.is_empty());
    }

    #[tokio::test]
    async fn a_failed_round_trip_whose_batch_is_already_running_is_not_retried() {
        let mut gateway = Scripted::new(
            [Err(unavailable())],
            [Ok(Some(after(1))), Ok(Some(after(2)))],
        );

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(gateway.submit_calls, 1);
        assert_eq!(reported, [after(1), after(2)]);
    }

    #[tokio::test]
    async fn progress_that_never_appears_is_reported_as_stalled() {
        let mut gateway = Scripted::new([Ok(())], [Ok(None)]);

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Stalled(None));
        assert_eq!(gateway.ticks, STALL_LIMIT);
        assert!(reported.is_empty());
    }

    #[tokio::test]
    async fn polls_that_keep_failing_stall_with_the_last_error() {
        let mut gateway = Scripted::new([Ok(())], [Err(unavailable())]);

        let (outcome, _) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Stalled(Some(unavailable())));
    }

    #[tokio::test]
    async fn advancing_progress_renews_the_stall_budget_until_completion() {
        // Each stretch of no change is one poll short of the budget; without
        // the renewal the batch would be declared stalled twice over.
        let progress = repeat(Ok(None), STALL_LIMIT - 1)
            .chain([Ok(Some(queued()))])
            .chain(repeat(Ok(Some(queued())), STALL_LIMIT - 1))
            .chain([Ok(Some(after(1)))])
            .chain(repeat(Ok(Some(after(1))), STALL_LIMIT - 1))
            .chain([Ok(Some(after(2)))]);
        let mut gateway = Scripted::new([Ok(())], progress);

        let (outcome, reported) = run(&mut gateway).await;

        assert_eq!(outcome, BatchOutcome::Completed(after(2)));
        assert_eq!(reported, [queued(), after(1), after(2)]);
    }
}
