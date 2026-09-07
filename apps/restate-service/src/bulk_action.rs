//! The `BulkAction` workflow: one per dashboard batch, driving a merge, rebase, or branch
//! update across many pull requests and exposing its progress to the dashboard.

use std::{collections::VecDeque, fmt, num::NonZeroUsize, sync::Arc, time::Duration};

use dependaboard_core::{
    ActionOutcome, BatchProgress, BatchRecord, BulkActionKind, BulkRequest, CommandRequest,
    DependabotCommand, MergeRequest, PrTarget, RunningBatch, UpdateBranchRequest, UserId,
    unix_seconds,
};
use dependaboard_store::PrStore;
use restate_sdk::prelude::*;
use tracing::warn;

use crate::{
    handler::{HandlerOutcome, handler_cause, traced, traced_read},
    pull_request::PullRequestClient,
    store::{
        batch_record_failure, brief_store_retry_policy, persistent_store_retry_policy,
        store_failure, store_retry_policy,
    },
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
    /// Lists the batch as running in the projection, so the audit view can show it
    /// before it finishes and a dashboard that lost it can follow it again. A
    /// convenience, not the batch's truth: the store is given a short while to take it,
    /// and a store that does not fails this step alone.
    fn start_batch(
        &mut self,
        batch: &RunningBatch,
    ) -> impl Future<Output = HandlerResult<()>> + Send;
    /// Stops listing the batch as running without recording it, for a workflow that
    /// ends with no finished batch to record: cancelled, or failed past what a target's
    /// own verdict can carry. Best effort, as the listing was, though with the ordinary
    /// store budget rather than the listing's short one: nothing waits behind it.
    fn unlist_batch(&mut self) -> impl Future<Output = HandlerResult<()>> + Send;
    /// Writes the finished batch to the projection, where it outlives the workflow's
    /// retention. Written once per batch: the store keeps the first record and Restate
    /// journals the step, so neither a retry nor a replay writes a second. Nothing would
    /// redo this write, so every store failure is retried — the ones the store calls
    /// terminal too — and the step is never given up: a store that refuses the record
    /// stalls the workflow, in the Restate UI and, once the record has been pending past
    /// [`RECORD_PENDING_QUIETLY_FOR`], in the service log on every failed attempt, until
    /// an operator has put the store right. The only way this ends without writing is
    /// the operator cancelling the workflow.
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
        self.ctx
            .object_client::<PullRequestClient>(target.key().to_string())
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

    async fn start_batch(&mut self, batch: &RunningBatch) -> HandlerResult<()> {
        let store = self.store.clone();
        let batch = batch.clone();
        self.ctx
            .run(move || async move {
                store.start_batch(&batch).await.map_err(store_failure)?;
                Ok(())
            })
            .retry_policy(brief_store_retry_policy())
            .name("start-batch")
            .await?;
        Ok(())
    }

    async fn unlist_batch(&mut self) -> HandlerResult<()> {
        let store = self.store.clone();
        let batch_id = self.ctx.key().to_owned();
        self.ctx
            .run(move || async move {
                store.unlist_batch(&batch_id).await.map_err(store_failure)?;
                Ok(())
            })
            .retry_policy(store_retry_policy())
            .name("unlist-batch")
            .await?;
        Ok(())
    }

    async fn record_batch(&mut self, record: &BatchRecord) -> HandlerResult<()> {
        let store = self.store.clone();
        let record = record.clone();
        self.ctx
            .run(move || async move {
                store.record_batch(&record).await.map_err(|error| {
                    let failure = batch_record_failure(error);
                    warn_if_record_stuck(&record, unix_seconds(), &handler_cause(&failure));
                    failure
                })?;
                Ok(())
            })
            .retry_policy(persistent_store_retry_policy())
            .name("record-batch")
            .await?;
        Ok(())
    }
}

/// How long a finished batch's record may go unwritten before its failed attempts are
/// logged. A store away for a few seconds is what the retry is for; a record still
/// pending past this is a store that has wedged, and an operator should not need the
/// Restate UI to see it.
const RECORD_PENDING_QUIETLY_FOR: Duration = Duration::from_secs(30);

/// Logs, at `warn`, a failed attempt to write a finished batch's record, once the record
/// has been pending for longer than [`RECORD_PENDING_QUIETLY_FOR`] since the batch
/// finished. `now` is Unix seconds; `cause` is the failure as Restate sees it, which
/// carries whether the store expected the failure to clear on its own. Restate owns the
/// retrying and re-runs the handler for each attempt, so no attempt can count the ones
/// before it; the record's age, from the journaled `completed_at`, is what every attempt
/// knows.
fn warn_if_record_stuck(record: &BatchRecord, now: u64, cause: &impl fmt::Display) {
    let pending_for = now.saturating_sub(record.completed_at);
    if pending_for <= RECORD_PENDING_QUIETLY_FOR.as_secs() {
        return;
    }
    warn!(
        batch_id = %record.batch_id,
        pending_for_seconds = pending_for,
        cause = %cause,
        "the finished batch's record has not reached the store; still retrying"
    );
}

/// Drives every target of the batch to a terminal state, writes the finished batch to
/// the projection, and resolves to the final tally.
///
/// The batch is first listed as running in the projection, so the audit view shows it
/// from its first moment and a dashboard that lost it can find it; a store that will
/// not take the listing is logged and the batch goes on without it, since the listing
/// is for finding the batch and not the batch itself. The finished record takes the
/// listing away; a workflow that ends without one — cancelled, or failed past what a
/// target's own verdict can carry — takes it away itself on the way out, so nothing is
/// listed as running for good. Merges go out in the rounds `plan_merge_rounds` lays
/// out. Branch updates go out
/// [`MAX_CONCURRENT`] at a time in batch order: updating one head branch moves nothing
/// under another, so same-repository targets need no serialising. Rebases go one at a
/// time with a pause between comments. `publish` is called with each change in progress,
/// so the dashboard sees every target settle as it happens rather than when the batch is
/// over. A target that fails terminally is recorded as failed with its reason and the
/// batch carries on: one pull request's problem is not a reason to leave the rest queued.
/// Once every target has settled the batch is recorded, with who asked for it and when
/// it ran, so its outcome is still there after Restate has forgotten the workflow. The
/// record is retried until it lands, whatever the store's failure, so the batch ends
/// with its tally rather than an error however long the store is away or wrong; a
/// batch that has completed and still ends in error was cancelled by an operator after
/// its last target settled — while stalled on the store, most likely — and it keeps its
/// running listing as evidence rather than vanishing from the audit view altogether.
async fn run_bulk_action<E: BulkActionEffects>(
    restate: &mut E,
    request: &BulkRequest,
    mut publish: impl FnMut(&BatchProgress) + Send,
) -> HandlerResult<BatchProgress> {
    let started_at = restate.now("batch-start-clock").await?;
    let mut progress = BatchProgress::queued(restate.batch_id(), request.action, &request.targets);
    publish(&progress);
    let running = RunningBatch {
        batch_id: progress.batch_id.clone(),
        action: request.action,
        requested_by: request.user_id.clone(),
        retried_from: request.retried_from.clone(),
        started_at,
        target_count: request.targets.len() as u64,
    };
    if let Err(error) = restate.start_batch(&running).await {
        warn!(
            batch_id = %running.batch_id,
            cause = %handler_cause(&error),
            "the running batch could not be listed in the projection; running it unlisted"
        );
    }
    let outcome = drive(restate, request, &mut progress, &mut publish, started_at).await;
    if outcome.is_err()
        && !progress.completed
        && let Err(error) = restate.unlist_batch().await
    {
        warn!(
            batch_id = %running.batch_id,
            cause = %handler_cause(&error),
            "the batch ended unfinished and could not be unlisted in the projection"
        );
    }
    outcome.map(|()| progress)
}

/// Runs the batch's targets and records the finished batch; the body of
/// [`run_bulk_action`] between listing the batch and, if this fails before the batch has
/// completed, unlisting it.
async fn drive<E: BulkActionEffects>(
    restate: &mut E,
    request: &BulkRequest,
    progress: &mut BatchProgress,
    publish: &mut (impl FnMut(&BatchProgress) + Send),
    started_at: u64,
) -> HandlerResult<()> {
    match request.action {
        BulkActionKind::Merge => {
            for round in plan_merge_rounds(&request.targets, MAX_CONCURRENT) {
                start_round(progress, &round, publish);
                restate
                    .merge_round(&round, |index, outcome| {
                        settle(progress, &round[index], outcome);
                        publish(progress);
                    })
                    .await?;
            }
        }
        BulkActionKind::UpdateBranch => {
            for round in request.targets.chunks(MAX_CONCURRENT.get()) {
                start_round(progress, round, publish);
                restate
                    .update_branch_round(round, |index, outcome| {
                        settle(progress, &round[index], outcome);
                        publish(progress);
                    })
                    .await?;
            }
        }
        BulkActionKind::Rebase => {
            for target in &request.targets {
                progress.start(&target.key());
                publish(progress);
                let outcome = restate.rebase(target).await;
                settle(progress, target, outcome);
                publish(progress);
                if !progress.completed {
                    restate.pause().await?;
                }
            }
        }
    }
    let completed_at = restate.now("batch-finish-clock").await?;
    let record = progress
        .completed_record(
            request.user_id.clone(),
            request.retried_from.clone(),
            started_at,
            completed_at,
        )
        .ok_or_else(|| {
            TerminalError::new("every target was attempted, yet the batch is not complete")
        })?;
    restate.record_batch(&record).await?;
    Ok(())
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
            request
                .validate(ctx.key())
                .map_err(|invalid| TerminalError::new(invalid.to_string()))?;
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
/// round takes the next pull request from up to `max_concurrent` repositories, going
/// round the repositories in the order the batch first names them and picking up where
/// the last round left off, so no repository waits on another's being drained. Every
/// target lands in exactly one round.
fn plan_merge_rounds(targets: &[PrTarget], max_concurrent: NonZeroUsize) -> Vec<Vec<PrTarget>> {
    // Each repository's pull requests in batch order, the repositories in the order the
    // batch first names them.
    let mut queues: Vec<(u64, VecDeque<PrTarget>)> = Vec::new();
    for target in targets {
        match queues
            .iter_mut()
            .find(|(repository_id, _)| *repository_id == target.repository_id)
        {
            Some((_, queue)) => queue.push_back(target.clone()),
            None => queues.push((target.repository_id, VecDeque::from([target.clone()]))),
        }
    }
    let mut rounds = Vec::new();
    let mut start = 0;
    loop {
        let mut round = Vec::new();
        let mut visited = 0;
        while visited < queues.len() && round.len() < max_concurrent.get() {
            let index = (start + visited) % queues.len();
            if let Some(target) = queues[index].1.pop_front() {
                round.push(target);
            }
            visited += 1;
        }
        if round.is_empty() {
            return rounds;
        }
        start = (start + visited) % queues.len();
        rounds.push(round);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dependaboard_core::{
        RejectReason, RunningBatch, TargetOutcome, TargetProgressState, UserId,
    };
    use tracing::instrument::WithSubscriber;

    use super::*;
    use crate::test_support::{LogSink, captured_logs, target};

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
            retried_from: None,
        }
    }

    /// The commit every merge in these tests makes.
    const MERGE_SHA: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6";

    fn merged() -> ActionOutcome {
        ActionOutcome::Succeeded {
            detail: "merged".to_owned(),
            merge_sha: Some(MERGE_SHA.to_owned()),
        }
    }

    fn updated() -> ActionOutcome {
        ActionOutcome::Succeeded {
            detail: "Updating pull request branch.".to_owned(),
            merge_sha: None,
        }
    }

    fn commented() -> ActionOutcome {
        ActionOutcome::Succeeded {
            detail: "@dependabot rebase posted".to_owned(),
            merge_sha: None,
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
        /// Every running batch listed in the projection, in order.
        started: Vec<RunningBatch>,
        /// What had been sent when the batch was listed as running, if it was.
        sent_when_started: Option<Vec<u64>>,
        /// Whether the store refuses to list the batch as running.
        start_fails: bool,
        /// Whether the first round ends the workflow, as a cancellation does.
        round_fails: bool,
        /// A failure the record step ends with instead of writing, taken once.
        record_failure: Option<HandlerError>,
        /// Every batch id whose running listing was taken away, in order.
        unlisted: Vec<String>,
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
            if self.round_fails {
                return Err(TerminalError::new("cancelled"));
            }
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

        async fn start_batch(&mut self, batch: &RunningBatch) -> HandlerResult<()> {
            if self.start_fails {
                return Err(TerminalError::new("database is locked").into());
            }
            self.started.push(batch.clone());
            self.sent_when_started = Some(self.sent.clone());
            Ok(())
        }

        async fn unlist_batch(&mut self) -> HandlerResult<()> {
            self.unlisted.push(self.batch_id().to_owned());
            Ok(())
        }

        async fn record_batch(&mut self, record: &BatchRecord) -> HandlerResult<()> {
            if let Some(failure) = self.record_failure.take() {
                return Err(failure);
            }
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
                detail: "merged".to_owned(),
                merge_sha: Some(MERGE_SHA.to_owned()),
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
                    detail: "merged".to_owned(),
                    merge_sha: Some(MERGE_SHA.to_owned()),
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
                detail: "merged".to_owned(),
                merge_sha: Some(MERGE_SHA.to_owned()),
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
                detail: "Updating pull request branch.".to_owned(),
                merge_sha: None,
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
    /// requester, the batch it retried if any, when the batch started and finished, the
    /// tally, and every verdict — a merge's with the commit it made.
    #[tokio::test]
    async fn a_finished_batch_is_recorded_in_the_projection_once_with_its_tally_and_verdicts() {
        let request = BulkRequest {
            user_id: UserId::new("alice"),
            retried_from: Some("batch-0".to_owned()),
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
                .completed_record(
                    UserId::new("alice"),
                    Some("batch-0".to_owned()),
                    CLOCK_EPOCH,
                    CLOCK_EPOCH + 60,
                )
                .expect("the batch has finished"),
            "the record is the finished progress, stamped with who asked, what it retried, and when"
        );
        assert_eq!(record.batch_id, "batch-1");
        assert_eq!(record.retried_from.as_deref(), Some("batch-0"));
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
                        detail: "merged".to_owned(),
                        merge_sha: Some(MERGE_SHA.to_owned()),
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

    /// A dashboard that lost the batch, or never followed it, finds it in the audit
    /// view while it runs: the workflow lists it as running before it sends the first
    /// target, with what was asked, by whom, which batch it retries if any, since when,
    /// and over how many pull requests.
    #[tokio::test]
    async fn a_batch_is_listed_as_running_before_its_first_target_is_sent() {
        let request = BulkRequest {
            user_id: UserId::new("alice"),
            retried_from: Some("batch-0".to_owned()),
            ..request(BulkActionKind::Merge, vec![pull(7, 1), pull(8, 4)])
        };
        let mut restate = RecordedBulkAction::default();

        run(&mut restate, &request).await;

        assert_eq!(
            restate.started,
            vec![RunningBatch {
                batch_id: "batch-1".to_owned(),
                action: BulkActionKind::Merge,
                requested_by: UserId::new("alice"),
                retried_from: Some("batch-0".to_owned()),
                started_at: CLOCK_EPOCH,
                target_count: 2,
            }]
        );
        assert_eq!(
            restate.sent_when_started,
            Some(Vec::new()),
            "listed before the first target went out"
        );
    }

    /// The running row is a convenience for the audit view, not the batch's truth; a
    /// store that will not take it holds up no merge. The batch runs, is recorded at
    /// the end as any other, and the store's refusal is in the service log.
    #[tokio::test]
    async fn a_batch_whose_running_row_cannot_be_written_still_runs_and_is_recorded() {
        let request = request(BulkActionKind::Merge, vec![pull(7, 1)]);
        let mut restate = RecordedBulkAction {
            start_fails: true,
            ..Default::default()
        };

        let logs = LogSink::default();
        let (progress, _) = run(&mut restate, &request)
            .with_subscriber(logs.subscriber())
            .await;

        assert!(progress.completed);
        assert_eq!(restate.sent, vec![1]);
        assert_eq!(restate.recorded.len(), 1, "{:?}", restate.recorded);
        let logs = logs.contents();
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("batch_id=batch-1"), "{logs}");
        assert!(
            logs.contains("the running batch could not be listed"),
            "{logs}"
        );
    }

    /// A batch cancelled in Restate, or failed past what a target's own verdict can
    /// carry, ends with no finished batch to record. It must not stay listed as
    /// running for good: the listing is taken away on the way out, and nothing is
    /// recorded in its place.
    #[tokio::test]
    async fn a_batch_that_ends_without_finishing_is_unlisted_and_not_recorded() {
        let request = request(BulkActionKind::Merge, vec![pull(7, 1), pull(8, 4)]);
        let mut restate = RecordedBulkAction {
            round_fails: true,
            ..Default::default()
        };

        let outcome = run_bulk_action(&mut restate, &request, |_| {}).await;

        assert!(outcome.is_err(), "the cancellation ends the workflow");
        assert_eq!(restate.started.len(), 1);
        assert_eq!(restate.unlisted, vec!["batch-1"]);
        assert!(restate.recorded.is_empty(), "{:?}", restate.recorded);
    }

    /// The record step retries every store failure, so the one way it ends without
    /// writing is an operator cancelling the workflow while it is stalled on the store.
    /// Every target has settled by then and the merges stand on GitHub; the running
    /// listing is the last evidence of the batch, and it is kept rather than taken away.
    #[tokio::test]
    async fn a_completed_batch_whose_record_step_is_cancelled_keeps_its_running_listing() {
        let request = request(BulkActionKind::Merge, vec![pull(7, 1), pull(8, 4)]);
        let mut restate = RecordedBulkAction {
            record_failure: Some(TerminalError::new("cancelled").into()),
            ..Default::default()
        };

        let outcome = run_bulk_action(&mut restate, &request, |_| {}).await;

        assert!(outcome.is_err(), "the cancellation ends the workflow");
        assert_eq!(restate.sent, vec![1, 4], "every target had settled");
        assert!(restate.recorded.is_empty(), "{:?}", restate.recorded);
        assert!(
            restate.unlisted.is_empty(),
            "the running row is kept as evidence: {:?}",
            restate.unlisted
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

    /// A finished one-target batch, completed a minute after the fake clock's epoch.
    fn finished_record() -> BatchRecord {
        let targets = [pull(7, 1)];
        let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
        progress.record(&targets[0].key(), merged());
        progress
            .completed_record(UserId::new("alice"), None, CLOCK_EPOCH, CLOCK_EPOCH + 60)
            .expect("the batch has finished")
    }

    /// Restate retries the record step for as long as it takes and re-runs the handler
    /// each time, so no attempt can count the ones before it; the record's age since the
    /// batch finished is what tells a blip from a store that has wedged.
    #[test]
    fn a_record_still_pending_past_the_grace_period_is_warned_about_with_its_age() {
        let record = finished_record();
        let now = record.completed_at + RECORD_PENDING_QUIETLY_FOR.as_secs() + 1;

        let logs = captured_logs(|| {
            warn_if_record_stuck(&record, now, &"database is locked");
        });

        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("batch_id=batch-1"), "{logs}");
        assert!(logs.contains("pending_for_seconds=31"), "{logs}");
        assert!(logs.contains("cause=database is locked"), "{logs}");
    }

    #[test]
    fn a_record_that_has_only_just_failed_to_land_is_retried_quietly() {
        let record = finished_record();

        let logs = captured_logs(|| {
            warn_if_record_stuck(&record, record.completed_at + 5, &"database is locked");
            warn_if_record_stuck(
                &record,
                record.completed_at + RECORD_PENDING_QUIETLY_FOR.as_secs(),
                &"database is locked",
            );
        });

        assert!(logs.is_empty(), "{logs}");
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

    /// Each round picks up where the last left off, so with more repositories than the
    /// bound, the fourth gets its turn in the second round rather than once the first
    /// three have nothing left.
    #[test]
    fn rounds_go_round_the_repositories_so_none_waits_for_the_others_to_drain() {
        let two_per_repository = (1..=4)
            .flat_map(|repository_id| [pull(repository_id, 1), pull(repository_id, 2)])
            .collect::<Vec<_>>();

        let rounds = plan_merge_rounds(&two_per_repository, MAX_CONCURRENT);

        assert_eq!(
            rounds,
            vec![
                vec![pull(1, 1), pull(2, 1), pull(3, 1)],
                vec![pull(4, 1), pull(1, 2), pull(2, 2)],
                vec![pull(3, 2), pull(4, 2)],
            ]
        );
    }

    /// Repositories take their turns in the order the batch first names them, not by
    /// id, so the user's ordering of the batch is the order the merges go out in.
    #[test]
    fn repositories_take_turns_in_the_order_the_batch_first_names_them() {
        let rounds = plan_merge_rounds(
            &[pull(9, 1), pull(3, 1), pull(9, 2), pull(3, 2)],
            NonZeroUsize::new(1).unwrap(),
        );

        assert_eq!(
            rounds,
            vec![
                vec![pull(9, 1)],
                vec![pull(3, 1)],
                vec![pull(9, 2)],
                vec![pull(3, 2)],
            ]
        );
    }

    #[test]
    fn an_empty_batch_plans_no_rounds() {
        assert!(plan_merge_rounds(&[], MAX_CONCURRENT).is_empty());
    }
}
