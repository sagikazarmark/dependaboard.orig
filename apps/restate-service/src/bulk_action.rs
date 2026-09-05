//! The `BulkAction` workflow: one per dashboard batch, driving a merge, rebase, or branch
//! update across many pull requests and exposing its progress to the dashboard.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use dependaboard_core::{
    ActionOutcome, BatchProgress, BatchRecord, BulkActionKind, BulkRequest, CommandRequest,
    DependabotCommand, MAX_BATCH_TARGETS, MergeRequest, PrTarget, UpdateBranchRequest, UserId,
    unix_seconds, valid_batch_id,
};
use dependaboard_store::PrStore;
use restate_sdk::prelude::*;
use tracing::warn;

use crate::{
    handler::{HandlerOutcome, traced, traced_read},
    pull_request::PullRequestClient,
    store::{persistent_store_retry_policy, store_failure},
};

const BATCH_PROGRESS: &str = "progress";
/// How many pull requests one merge or branch-update round sends to GitHub at once.
const MAX_CONCURRENT: NonZeroUsize = NonZeroUsize::new(3).unwrap();
/// How long a rebase batch waits between two Dependabot comments, so a run of them does
/// not trip GitHub's secondary rate limit on content creation.
const COMMENT_SPACING: Duration = Duration::from_millis(350);

#[derive(Clone)]
pub(crate) struct BulkAction {
    pub(crate) store: Arc<dyn PrStore>,
}

impl HandlerOutcome for Json<BatchProgress> {
    fn outcome(&self) -> String {
        let progress = &self.0;
        format!(
            "{} succeeded, {} rejected, {} failed of {} targets",
            progress.succeeded,
            progress.rejected,
            progress.failed,
            progress.targets.len()
        )
    }
}

/// The calls a bulk action asks Restate to make, abstracted so `run_bulk_action` can be
/// exercised against a recording fake without a runtime.
///
/// A Restate-to-Restate call only fails once the callee has failed terminally; retryable
/// failures are retried inside the `PullRequest` handler and never surface here.
trait BulkActionEffects {
    /// The workflow key, which is also what the progress and every per-target request carry.
    fn batch_id(&self) -> &str;
    /// Sends one round of merges to GitHub at once and hands each outcome to `landed`, with
    /// the target's index in `targets`, as it arrives rather than once the round is over.
    fn merge_round(
        &mut self,
        targets: &[PrTarget],
        landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>) + Send,
    ) -> impl Future<Output = Result<(), TerminalError>> + Send;
    /// Sends one round of branch updates to GitHub at once, landing outcomes the way
    /// `merge_round` does.
    fn update_branch_round(
        &mut self,
        targets: &[PrTarget],
        landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>) + Send,
    ) -> impl Future<Output = Result<(), TerminalError>> + Send;
    /// Asks Dependabot to rebase one pull request; resolves once the comment is posted.
    fn rebase(
        &mut self,
        target: &PrTarget,
    ) -> impl Future<Output = Result<ActionOutcome, TerminalError>> + Send;
    /// Waits [`COMMENT_SPACING`] durably between two comments.
    fn pause(&mut self) -> impl Future<Output = Result<(), TerminalError>> + Send;
    /// The wall clock, in Unix seconds, journaled under `step` so a replay reads the
    /// same moment.
    fn now(&mut self, step: &'static str) -> impl Future<Output = HandlerResult<u64>> + Send;
    /// Writes the finished batch to the projection, where it outlives the workflow's
    /// retention. Written once per batch: the store keeps the first record and Restate
    /// journals the step, so neither a retry nor a replay writes a second. Nothing would
    /// redo this write, so it is retried until the store takes it rather than given up.
    fn record_batch(
        &mut self,
        record: &BatchRecord,
    ) -> impl Future<Output = HandlerResult<()>> + Send;
}

struct RestateBulkAction<'a, 'ctx> {
    ctx: &'a WorkflowContext<'ctx>,
    store: &'a Arc<dyn PrStore>,
    user_id: UserId,
}

impl RestateBulkAction<'_, '_> {
    fn pull_request(&self, target: &PrTarget) -> PullRequestClient<'_> {
        self.ctx.object_client::<PullRequestClient>(target.key())
    }
}

/// Awaits every call of one round, handing each outcome to `landed` with the call's index
/// as it arrives.
async fn land_round<C>(
    calls: impl IntoIterator<Item = C>,
    mut landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>) + Send,
) -> Result<(), TerminalError>
where
    C: CallFuture<Response = Json<ActionOutcome>> + Send,
{
    let mut calls = calls.into_iter().collect::<DurableFuturesUnordered<_>>();
    while let Some((index, outcome)) = calls.next().await? {
        landed(index, outcome.map(Json::into_inner));
    }
    Ok(())
}

impl BulkActionEffects for RestateBulkAction<'_, '_> {
    fn batch_id(&self) -> &str {
        self.ctx.key()
    }

    async fn merge_round(
        &mut self,
        targets: &[PrTarget],
        landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>) + Send,
    ) -> Result<(), TerminalError> {
        let calls = targets.iter().map(|target| {
            self.pull_request(target)
                .merge(Json::from(MergeRequest {
                    batch_id: self.ctx.key().to_owned(),
                    target: target.clone(),
                }))
                .call()
        });
        land_round(calls, landed).await
    }

    async fn update_branch_round(
        &mut self,
        targets: &[PrTarget],
        landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>) + Send,
    ) -> Result<(), TerminalError> {
        let calls = targets.iter().map(|target| {
            self.pull_request(target)
                .update_branch(Json::from(UpdateBranchRequest {
                    batch_id: self.ctx.key().to_owned(),
                    target: target.clone(),
                }))
                .call()
        });
        land_round(calls, landed).await
    }

    async fn rebase(&mut self, target: &PrTarget) -> Result<ActionOutcome, TerminalError> {
        self.pull_request(target)
            .command(Json::from(CommandRequest {
                batch_id: self.ctx.key().to_owned(),
                target: target.clone(),
                user_id: self.user_id.clone(),
                command: DependabotCommand::Rebase,
            }))
            .call()
            .await
            .map(Json::into_inner)
    }

    async fn pause(&mut self) -> Result<(), TerminalError> {
        self.ctx.sleep(COMMENT_SPACING).await
    }

    async fn now(&mut self, step: &'static str) -> HandlerResult<u64> {
        Ok(self
            .ctx
            .run(|| async { Ok(unix_seconds()) })
            .name(step)
            .await?)
    }

    async fn record_batch(&mut self, record: &BatchRecord) -> HandlerResult<()> {
        let store = self.store.clone();
        let record = record.clone();
        self.ctx
            .run(move || async move {
                store.record_batch(&record).await.map_err(store_failure)?;
                Ok(())
            })
            .retry_policy(persistent_store_retry_policy())
            .name("record-batch")
            .await?;
        Ok(())
    }
}

/// Drives every target of the batch to a terminal state, writes the finished batch to
/// the projection, and resolves to the final tally.
///
/// Merges go out in the rounds `plan_merge_rounds` lays out. Branch updates go out
/// [`MAX_CONCURRENT`] at a time in batch order: updating one head branch moves nothing
/// under another, so same-repository targets need no serialising. Rebases go one at a
/// time with a pause between comments. `publish` is called with each change in progress,
/// so the dashboard sees every target settle as it happens rather than when the batch is
/// over. A target that fails terminally is recorded as failed with its reason and the
/// batch carries on: one pull request's problem is not a reason to leave the rest queued.
/// Once every target has settled the batch is recorded, with who asked for it and when
/// it ran, so its outcome is still there after Restate has forgotten the workflow. The
/// record is retried until it lands, so the batch still ends with its tally rather than
/// an error when the store is away for a while; only a store that rejects the record
/// outright fails it.
async fn run_bulk_action<E: BulkActionEffects>(
    restate: &mut E,
    request: &BulkRequest,
    mut publish: impl FnMut(&BatchProgress) + Send,
) -> HandlerResult<BatchProgress> {
    let started_at = restate.now("batch-start-clock").await?;
    let mut progress = BatchProgress::queued(restate.batch_id(), request.action, &request.targets);
    publish(&progress);
    match request.action {
        BulkActionKind::Merge => {
            for round in plan_merge_rounds(&request.targets, MAX_CONCURRENT) {
                start_round(&mut progress, &round, &mut publish);
                restate
                    .merge_round(&round, |index, outcome| {
                        settle(&mut progress, &round[index], outcome);
                        publish(&progress);
                    })
                    .await?;
            }
        }
        BulkActionKind::UpdateBranch => {
            for round in request.targets.chunks(MAX_CONCURRENT.get()) {
                start_round(&mut progress, round, &mut publish);
                restate
                    .update_branch_round(round, |index, outcome| {
                        settle(&mut progress, &round[index], outcome);
                        publish(&progress);
                    })
                    .await?;
            }
        }
        BulkActionKind::Rebase => {
            for target in &request.targets {
                progress.start(&target.key());
                publish(&progress);
                let outcome = restate.rebase(target).await;
                settle(&mut progress, target, outcome);
                publish(&progress);
                if !progress.completed {
                    restate.pause().await?;
                }
            }
        }
    }
    let completed_at = restate.now("batch-finish-clock").await?;
    let record = progress
        .completed_record(request.user_id.clone(), started_at, completed_at)
        .ok_or_else(|| {
            TerminalError::new("every target was attempted, yet the batch is not complete")
        })?;
    restate.record_batch(&record).await?;
    Ok(progress)
}

/// Marks every target of a round as running and publishes once for the round, so the
/// dashboard shows the whole round in flight together.
fn start_round(
    progress: &mut BatchProgress,
    round: &[PrTarget],
    publish: &mut impl FnMut(&BatchProgress),
) {
    for target in round {
        progress.start(&target.key());
    }
    publish(progress);
}

/// Records how one target's action ended. A terminal failure is that target's alone: it is
/// noted with its reason, logged, and the batch moves on.
fn settle(
    progress: &mut BatchProgress,
    target: &PrTarget,
    outcome: Result<ActionOutcome, TerminalError>,
) {
    let key = target.key();
    match outcome {
        Ok(outcome) => progress.record(&key, outcome),
        Err(error) => {
            warn!(
                batch_id = %progress.batch_id,
                pull_request = %key,
                cause = %error,
                "target failed terminally; continuing the batch"
            );
            progress.record_failure(&key, error.message());
        }
    }
}

#[restate_sdk::workflow(workflow_completion_retention = "7 days")]
impl BulkAction {
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<BulkRequest>,
    ) -> HandlerResult<Json<BatchProgress>> {
        traced("BulkAction/run", ctx.key(), async {
            let request = request.into_inner();
            validate_batch_request(ctx.key(), &request)?;
            let mut restate = RestateBulkAction {
                ctx: &ctx,
                store: &self.store,
                user_id: request.user_id.clone(),
            };
            let progress = run_bulk_action(&mut restate, &request, |progress| {
                ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
            })
            .await?;
            Ok(Json::from(progress))
        })
        .await
    }

    #[handler]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> HandlerResult<Json<Option<BatchProgress>>> {
        traced_read("BulkAction/progress", ctx.key(), async {
            Ok(Json::from(
                ctx.get::<Json<BatchProgress>>(BATCH_PROGRESS)
                    .await?
                    .map(Json::into_inner),
            ))
        })
        .await
    }
}

/// Splits a merge batch into the rounds `run_bulk_action` sends to GitHub together.
///
/// Two pull requests in one repository never share a round: merging the first moves the
/// base branch under the second, so they go one round after another in batch order. Each
/// round takes the next pull request from up to `max_concurrent` repositories, visiting
/// repositories in id order. Every target lands in exactly one round.
fn plan_merge_rounds(targets: &[PrTarget], max_concurrent: NonZeroUsize) -> Vec<Vec<PrTarget>> {
    let mut by_repository: BTreeMap<u64, VecDeque<PrTarget>> = BTreeMap::new();
    for target in targets {
        by_repository
            .entry(target.repository_id)
            .or_default()
            .push_back(target.clone());
    }
    let mut rounds = Vec::new();
    loop {
        let round = by_repository
            .values_mut()
            .filter_map(VecDeque::pop_front)
            .take(max_concurrent.get())
            .collect::<Vec<_>>();
        if round.is_empty() {
            return rounds;
        }
        rounds.push(round);
    }
}

fn validate_batch_request(batch_id: &str, request: &BulkRequest) -> HandlerResult<()> {
    if !valid_batch_id(batch_id) {
        return Err(TerminalError::new("batch key must be a UUIDv7").into());
    }
    if request.targets.is_empty() || request.targets.len() > MAX_BATCH_TARGETS {
        return Err(TerminalError::new(format!(
            "batch must contain between 1 and {MAX_BATCH_TARGETS} targets"
        ))
        .into());
    }
    let mut keys = BTreeSet::new();
    if request
        .targets
        .iter()
        .any(|target| !keys.insert(target.key()))
    {
        return Err(TerminalError::new("batch contains duplicate pull requests").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{RejectReason, TargetOutcome, TargetProgressState, UserId};

    use super::*;
    use crate::test_support::target;

    /// A merge target for pull request `number` in repository `repository_id`.
    fn pull(repository_id: u64, number: u64) -> PrTarget {
        PrTarget {
            repository_id,
            number,
            ..target()
        }
    }

    fn request(action: BulkActionKind, targets: Vec<PrTarget>) -> BulkRequest {
        BulkRequest {
            action,
            targets,
            user_id: UserId::new("dashboard"),
        }
    }

    fn merged() -> ActionOutcome {
        ActionOutcome::Succeeded {
            detail: "merged".to_owned(),
        }
    }

    fn updated() -> ActionOutcome {
        ActionOutcome::Succeeded {
            detail: "Updating pull request branch.".to_owned(),
        }
    }

    fn commented() -> ActionOutcome {
        ActionOutcome::Succeeded {
            detail: "@dependabot rebase posted".to_owned(),
        }
    }

    fn forbidden() -> ActionOutcome {
        ActionOutcome::Rejected {
            reason: RejectReason::Forbidden,
        }
    }

    fn stale() -> ActionOutcome {
        ActionOutcome::Rejected {
            reason: RejectReason::StaleSha {
                expected: "abc123".to_owned(),
                actual: "def456".to_owned(),
            },
        }
    }

    fn github_500() -> TerminalError {
        TerminalError::new("GitHub mutation failed with HTTP 500: Internal Server Error")
    }

    /// Stands in for the `PullRequest` objects a batch calls and records what it sent them.
    ///
    /// Each pull request answers with its scripted outcome, or succeeds when none is
    /// scripted. A round lands its outcomes in reverse order of sending, so a batch that
    /// confused the round's indices would record outcomes against the wrong targets. The
    /// clock advances by a minute per reading, so a start and a finish never coincide.
    #[derive(Default)]
    struct RecordedBulkAction {
        answers: BTreeMap<u64, Result<ActionOutcome, TerminalError>>,
        /// Pull request numbers in the order their actions were sent.
        sent: Vec<u64>,
        /// The pull request numbers of each concurrent round, in the order sent.
        rounds: Vec<Vec<u64>>,
        pauses: u32,
        /// Minutes the clock has been read for, from a fixed epoch.
        clock_readings: u64,
        /// Every batch record written to the projection, in order.
        recorded: Vec<BatchRecord>,
    }

    /// The fake clock's first reading, in Unix seconds.
    const CLOCK_EPOCH: u64 = 1_700_000_000;

    impl RecordedBulkAction {
        fn answering(
            answers: impl IntoIterator<Item = (u64, Result<ActionOutcome, TerminalError>)>,
        ) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                ..Default::default()
            }
        }

        fn answer(
            &mut self,
            target: &PrTarget,
            succeeded: ActionOutcome,
        ) -> Result<ActionOutcome, TerminalError> {
            self.sent.push(target.number);
            self.answers.remove(&target.number).unwrap_or(Ok(succeeded))
        }

        fn round(
            &mut self,
            targets: &[PrTarget],
            succeeded: ActionOutcome,
            mut landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>),
        ) {
            self.rounds
                .push(targets.iter().map(|target| target.number).collect());
            let outcomes = targets
                .iter()
                .map(|target| self.answer(target, succeeded.clone()))
                .collect::<Vec<_>>();
            for (index, outcome) in outcomes.into_iter().enumerate().rev() {
                landed(index, outcome);
            }
        }
    }

    impl BulkActionEffects for RecordedBulkAction {
        fn batch_id(&self) -> &str {
            "batch-1"
        }

        async fn merge_round(
            &mut self,
            targets: &[PrTarget],
            landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>) + Send,
        ) -> Result<(), TerminalError> {
            self.round(targets, merged(), landed);
            Ok(())
        }

        async fn update_branch_round(
            &mut self,
            targets: &[PrTarget],
            landed: impl FnMut(usize, Result<ActionOutcome, TerminalError>) + Send,
        ) -> Result<(), TerminalError> {
            self.round(targets, updated(), landed);
            Ok(())
        }

        async fn rebase(&mut self, target: &PrTarget) -> Result<ActionOutcome, TerminalError> {
            self.answer(target, commented())
        }

        async fn pause(&mut self) -> Result<(), TerminalError> {
            self.pauses += 1;
            Ok(())
        }

        async fn now(&mut self, _step: &'static str) -> HandlerResult<u64> {
            let reading = CLOCK_EPOCH + self.clock_readings * 60;
            self.clock_readings += 1;
            Ok(reading)
        }

        async fn record_batch(&mut self, record: &BatchRecord) -> HandlerResult<()> {
            self.recorded.push(record.clone());
            Ok(())
        }
    }

    async fn run(
        restate: &mut RecordedBulkAction,
        request: &BulkRequest,
    ) -> (BatchProgress, Vec<BatchProgress>) {
        let mut published = Vec::new();
        let progress = run_bulk_action(restate, request, |progress| {
            published.push(progress.clone());
        })
        .await
        .unwrap();
        (progress, published)
    }

    fn state_of(progress: &BatchProgress, number: u64) -> &TargetProgressState {
        &progress
            .targets
            .iter()
            .find(|target| target.target.number == number)
            .expect("the target is in the batch")
            .state
    }

    #[tokio::test]
    async fn a_target_that_fails_terminally_is_recorded_and_the_rest_of_the_batch_still_runs() {
        // Three pull requests in one repository merge one round after another, so an abort
        // on the second would have left the third queued forever.
        let request = request(
            BulkActionKind::Merge,
            vec![pull(7, 1), pull(7, 2), pull(7, 3)],
        );
        let mut restate = RecordedBulkAction::answering([(2, Err(github_500()))]);

        let (progress, _) = run(&mut restate, &request).await;

        assert_eq!(restate.sent, vec![1, 2, 3]);
        assert!(progress.completed, "the batch ran to the end");
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (2, 0, 1)
        );
        assert_eq!(
            state_of(&progress, 2),
            &TargetProgressState::Failed {
                detail: "GitHub mutation failed with HTTP 500: Internal Server Error".to_owned()
            },
            "the failed target carries the callee's reason, not the SDK's wrapping of it"
        );
        assert_eq!(
            state_of(&progress, 3),
            &TargetProgressState::Succeeded {
                detail: "merged".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_batch_whose_every_target_merges_completes_with_only_successes() {
        let request = request(
            BulkActionKind::Merge,
            vec![pull(7, 1), pull(8, 4), pull(9, 2)],
        );
        let mut restate = RecordedBulkAction::default();

        let (progress, _) = run(&mut restate, &request).await;

        assert!(progress.completed);
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (3, 0, 0)
        );
        assert!(
            progress.targets.iter().all(|target| target.state
                == TargetProgressState::Succeeded {
                    detail: "merged".to_owned()
                }),
            "{:?}",
            progress.targets
        );
    }

    #[tokio::test]
    async fn a_rejected_target_is_counted_apart_from_the_failed_ones() {
        let request = request(BulkActionKind::Merge, vec![pull(7, 1), pull(8, 4)]);
        let mut restate = RecordedBulkAction::answering([(4, Ok(forbidden()))]);

        let (progress, _) = run(&mut restate, &request).await;

        assert!(progress.completed);
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (1, 1, 0)
        );
        assert_eq!(
            state_of(&progress, 4),
            &TargetProgressState::Rejected {
                reason: RejectReason::Forbidden
            }
        );
    }

    #[tokio::test]
    async fn a_mixed_merge_round_settles_each_target_on_its_own() {
        // Three repositories merge in one concurrent round; each lands its own verdict.
        let request = request(
            BulkActionKind::Merge,
            vec![pull(7, 1), pull(8, 4), pull(9, 2)],
        );
        let mut restate =
            RecordedBulkAction::answering([(4, Ok(forbidden())), (2, Err(github_500()))]);

        let (progress, _) = run(&mut restate, &request).await;

        assert!(progress.completed);
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (1, 1, 1)
        );
        assert_eq!(
            state_of(&progress, 1),
            &TargetProgressState::Succeeded {
                detail: "merged".to_owned()
            }
        );
        assert_eq!(
            state_of(&progress, 4),
            &TargetProgressState::Rejected {
                reason: RejectReason::Forbidden
            }
        );
        assert_eq!(
            state_of(&progress, 2),
            &TargetProgressState::Failed {
                detail: "GitHub mutation failed with HTTP 500: Internal Server Error".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_mixed_rebase_batch_reports_every_column_and_paces_its_comments() {
        let request = request(
            BulkActionKind::Rebase,
            vec![pull(7, 1), pull(7, 2), pull(7, 3), pull(7, 4)],
        );
        let mut restate =
            RecordedBulkAction::answering([(2, Err(github_500())), (3, Ok(forbidden()))]);

        let (progress, _) = run(&mut restate, &request).await;

        assert_eq!(
            restate.sent,
            vec![1, 2, 3, 4],
            "the failure did not stop the rest"
        );
        assert!(progress.completed);
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (2, 1, 1)
        );
        assert_eq!(
            restate.pauses, 3,
            "one pause between each pair of comments and none after the last"
        );
    }

    #[tokio::test]
    async fn a_mixed_update_branch_batch_settles_each_target_on_its_own_and_never_pauses() {
        // Four pull requests in one repository: a stale head is rejected, a GitHub 500 is
        // failed, and neither stops the other two from having their branches updated.
        let request = request(
            BulkActionKind::UpdateBranch,
            vec![pull(7, 1), pull(7, 2), pull(7, 3), pull(7, 4)],
        );
        let mut restate = RecordedBulkAction::answering([(2, Err(github_500())), (3, Ok(stale()))]);

        let (progress, _) = run(&mut restate, &request).await;

        assert_eq!(restate.sent, vec![1, 2, 3, 4]);
        assert!(progress.completed);
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (2, 1, 1)
        );
        assert_eq!(
            state_of(&progress, 1),
            &TargetProgressState::Succeeded {
                detail: "Updating pull request branch.".to_owned()
            }
        );
        assert_eq!(
            state_of(&progress, 2),
            &TargetProgressState::Failed {
                detail: "GitHub mutation failed with HTTP 500: Internal Server Error".to_owned()
            }
        );
        assert_eq!(
            state_of(&progress, 3),
            &TargetProgressState::Rejected {
                reason: RejectReason::StaleSha {
                    expected: "abc123".to_owned(),
                    actual: "def456".to_owned(),
                }
            }
        );
        assert_eq!(
            restate.pauses, 0,
            "branch updates are App mutations, not comments, so they need no spacing"
        );
    }

    #[tokio::test]
    async fn branch_updates_go_out_in_rounds_of_the_concurrency_bound_in_batch_order() {
        // Updating one head branch moves nothing under another, so unlike merges, pull
        // requests in one repository share a round.
        let request = request(
            BulkActionKind::UpdateBranch,
            vec![pull(7, 1), pull(7, 2), pull(7, 3), pull(7, 4), pull(8, 5)],
        );
        let mut restate = RecordedBulkAction::default();

        let (progress, published) = run(&mut restate, &request).await;

        assert_eq!(restate.rounds, vec![vec![1, 2, 3], vec![4, 5]]);
        assert_eq!(
            (progress.succeeded, progress.rejected, progress.failed),
            (5, 0, 0)
        );
        let running_after_first_round_started = published[1]
            .targets
            .iter()
            .filter(|target| target.state == TargetProgressState::Running)
            .count();
        assert_eq!(
            running_after_first_round_started, 3,
            "the dashboard sees the whole round in flight, not one target at a time"
        );
    }

    #[tokio::test]
    async fn progress_is_published_as_each_target_settles_not_once_the_round_is_over() {
        // Three repositories merge in one round; the dashboard must see them land one by one.
        let request = request(
            BulkActionKind::Merge,
            vec![pull(7, 1), pull(8, 1), pull(9, 1)],
        );
        let mut restate = RecordedBulkAction::default();

        let (_, published) = run(&mut restate, &request).await;

        let settled_per_publish = published
            .iter()
            .map(|progress| progress.succeeded + progress.rejected + progress.failed)
            .collect::<Vec<_>>();
        assert_eq!(
            settled_per_publish,
            vec![0, 0, 1, 2, 3],
            "queued, the round running, then one more settled per publish"
        );
        assert!(
            published.last().is_some_and(|progress| progress.completed),
            "the last publish is the finished batch"
        );
    }

    /// Restate forgets the workflow after its retention; the projection is where the
    /// batch's outcome lives on. It is written once, as the batch's last step, with the
    /// requester, when the batch started and finished, the tally, and every verdict.
    #[tokio::test]
    async fn a_finished_batch_is_recorded_in_the_projection_once_with_its_tally_and_verdicts() {
        let request = BulkRequest {
            user_id: UserId::new("alice"),
            ..request(
                BulkActionKind::Merge,
                vec![pull(7, 1), pull(8, 4), pull(9, 2)],
            )
        };
        let mut restate =
            RecordedBulkAction::answering([(4, Ok(forbidden())), (2, Err(github_500()))]);

        let (progress, _) = run(&mut restate, &request).await;

        assert_eq!(restate.recorded.len(), 1, "{:?}", restate.recorded);
        let record = &restate.recorded[0];
        assert_eq!(
            record,
            &progress
                .completed_record(UserId::new("alice"), CLOCK_EPOCH, CLOCK_EPOCH + 60)
                .expect("the batch has finished"),
            "the record is the finished progress, stamped with who asked and when"
        );
        assert_eq!(record.batch_id, "batch-1");
        assert_eq!(
            (record.succeeded, record.rejected, record.failed),
            (1, 1, 1)
        );
        assert_eq!(
            record
                .targets
                .iter()
                .map(|target| (target.number, target.outcome.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    1,
                    TargetOutcome::Succeeded {
                        detail: "merged".to_owned()
                    }
                ),
                (
                    4,
                    TargetOutcome::Rejected {
                        reason: RejectReason::Forbidden
                    }
                ),
                (
                    2,
                    TargetOutcome::Failed {
                        detail: "GitHub mutation failed with HTTP 500: Internal Server Error"
                            .to_owned()
                    }
                ),
            ],
            "every target in batch order with its own verdict"
        );
    }

    #[test]
    fn the_completion_line_lists_every_column() {
        let targets = [pull(7, 1), pull(7, 2), pull(7, 3)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(&targets[0].key(), merged());
        progress.record(&targets[1].key(), forbidden());
        progress.record_failure(&targets[2].key(), "boom");

        assert_eq!(
            Json::from(progress).outcome(),
            "1 succeeded, 1 rejected, 1 failed of 3 targets"
        );
    }

    #[test]
    fn batch_validation_rejects_duplicate_targets() {
        let target = target();
        let request = BulkRequest {
            action: BulkActionKind::Merge,
            targets: vec![target.clone(), target],
            user_id: UserId::new("dashboard"),
        };
        assert!(validate_batch_request(&dependaboard_core::new_batch_id(), &request).is_err());
    }

    #[test]
    fn pull_requests_in_different_repositories_merge_in_the_same_round() {
        let rounds = plan_merge_rounds(&[pull(7, 1), pull(8, 1), pull(9, 1)], MAX_CONCURRENT);

        assert_eq!(rounds, vec![vec![pull(7, 1), pull(8, 1), pull(9, 1)]]);
    }

    #[test]
    fn pull_requests_in_one_repository_merge_one_round_after_another_in_batch_order() {
        let rounds = plan_merge_rounds(
            &[pull(7, 5), pull(7, 2), pull(8, 1), pull(7, 9)],
            MAX_CONCURRENT,
        );

        assert_eq!(
            rounds,
            vec![
                vec![pull(7, 5), pull(8, 1)],
                vec![pull(7, 2)],
                vec![pull(7, 9)],
            ],
            "merging one pull request moves the base under the next, so a repository never merges two at once"
        );
    }

    #[test]
    fn no_round_sends_more_pull_requests_than_the_concurrency_bound() {
        let one_per_repository = (1..=5)
            .map(|repository_id| pull(repository_id, 1))
            .collect::<Vec<_>>();

        let rounds = plan_merge_rounds(&one_per_repository, NonZeroUsize::new(2).unwrap());

        assert_eq!(
            rounds,
            vec![
                vec![pull(1, 1), pull(2, 1)],
                vec![pull(3, 1), pull(4, 1)],
                vec![pull(5, 1)],
            ]
        );
    }

    #[test]
    fn an_empty_batch_plans_no_rounds() {
        assert!(plan_merge_rounds(&[], MAX_CONCURRENT).is_empty());
    }
}
