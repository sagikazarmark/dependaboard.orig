//! The `PullRequest` virtual object: one per Dependabot pull request, owning its canonical
//! snapshot, its action history, and every GitHub mutation made on its behalf.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use dependaboard_core::{
    ActionLog, ActionOutcome, CommandRequest, MergeMethod, MergeRequest, Operation, PrKey,
    PrRecord, PrState, PrTarget, RejectReason, SyncRequest, UpdateBranchRequest, unix_seconds,
};
use dependaboard_github::{GithubError, Merged};
use dependaboard_store::{ProjectionWriter, StoreError};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    github::{
        GithubApiHandle, RestateGithubStep, Settled, action_result, merge_result, read_result,
        rejected, run_github_step,
    },
    handler::{HandlerOutcome, traced, traced_read},
    store::{StoreStepContext, store_failure, store_retry_policy},
};

const PR_STATE: &str = "pr_state";

#[derive(Clone)]
pub(crate) struct PullRequest {
    pub(crate) github: GithubApiHandle,
    pub(crate) store: Arc<dyn ProjectionWriter>,
    pub(crate) debounce: Duration,
}

/// What `PullRequest.closed` was told about why the pull request is gone.
///
/// A webhook knows GitHub closed it and sends nothing: an empty body is the unconditional
/// form, which also keeps `closed` sends journaled before this payload existed deliverable.
/// A sweep that pruned the row only knows the pull request was absent when its listing
/// started, so it names that instant: a pull request synced since then was reopened and
/// re-projected behind the sweep's back, and keeps its state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClosedRequest {
    pub(crate) synced_before: Option<u64>,
}

impl restate_sdk::serde::Serialize for ClosedRequest {
    type Error = serde_json::Error;

    fn serialize(&self) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(self).map(Bytes::from)
    }
}

impl restate_sdk::serde::Deserialize for ClosedRequest {
    type Error = serde_json::Error;

    fn deserialize(bytes: &mut Bytes) -> Result<Self, Self::Error> {
        if bytes.is_empty() {
            Ok(Self::default())
        } else {
            serde_json::from_slice(bytes)
        }
    }
}

impl restate_sdk::serde::PayloadMetadata for ClosedRequest {
    fn json_schema() -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "synced_before": { "type": ["integer", "null"], "minimum": 0 }
            }
        }))
    }
}

/// What `PullRequest.closed` did, for its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosedOutcome {
    /// Row and state are gone.
    Retired,
    /// A sweep's fence found the pull request synced since the sweep began; it is open again.
    Kept,
}

impl HandlerOutcome for ClosedOutcome {
    fn outcome(&self) -> String {
        match self {
            Self::Retired => "retired".to_owned(),
            Self::Kept => "kept; synced since the sweep began".to_owned(),
        }
    }
}

/// What one `PullRequest.sync` attempt did, for its log line.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncOutcome {
    /// Coalesced behind the trailing sync that is (now) scheduled.
    Debounced,
    Synced {
        head_sha: String,
    },
    /// GitHub no longer serves the pull request; its row and state are gone.
    Deleted,
}

impl HandlerOutcome for SyncOutcome {
    fn outcome(&self) -> String {
        match self {
            Self::Debounced => "debounced".to_owned(),
            Self::Synced { head_sha } => format!("synced at {}", short_sha(head_sha)),
            Self::Deleted => "deleted; no longer an open Dependabot pull request".to_owned(),
        }
    }
}

/// The calls a pull request's handlers ask of Restate, GitHub and the store, abstracted so
/// the `run_*` functions can be exercised against a recording fake without a runtime.
///
/// State reads and writes and one-way sends are Restate's own and need no journaling; the
/// clock, the GitHub steps and the store steps are journaled under the names the `Restate*`
/// impl gives them, which in-flight invocations replay by.
trait PullRequestEffects {
    /// The object key, `{repository_id}#{number}`.
    fn key(&self) -> &str;
    /// Whether a user identity is configured to post `@dependabot` commands under.
    fn can_post_commands(&self) -> bool;
    /// The wall clock, in Unix seconds, journaled under `step` so a replay reads the
    /// same moment.
    fn now(&mut self, step: &'static str) -> impl Future<Output = HandlerResult<u64>> + Send;
    /// The object's durable state, if any handler has set it.
    fn state(&mut self) -> impl Future<Output = HandlerResult<Option<PrState>>> + Send;
    fn set_state(&mut self, state: PrState);
    /// Forgets the object's state entirely; the next handler finds a fresh object.
    fn clear_state(&mut self);
    /// Sends `request` one-way to the object its key names, to run `after` a delay, or as
    /// soon as the object is free when `None`. Every sender keys a sync by its request, so
    /// the object a handler here addresses is its own.
    fn schedule_sync(&mut self, request: SyncRequest, after: Option<Duration>);
    /// Reads the pull request's canonical snapshot from GitHub: `Ok(None)` for one that is
    /// open but not Dependabot's, and a 404 settled as rejected not found for one that is
    /// gone. `known_resource` says the object has read it before, so that 404 is the pull
    /// request gone rather than a permissions misconfiguration.
    fn fetch_snapshot(
        &mut self,
        request: &SyncRequest,
        known_resource: bool,
    ) -> impl Future<Output = HandlerResult<Settled<Option<PrRecord>>>> + Send;
    /// Merges the pull request with `merge_method`, or the configured preference when `None`.
    fn merge(
        &mut self,
        request: &MergeRequest,
        merge_method: Option<MergeMethod>,
        known_resource: bool,
    ) -> impl Future<Output = HandlerResult<Settled<Merged>>> + Send;
    /// Posts the `@dependabot` command under the user identity.
    fn post_command(
        &mut self,
        request: &CommandRequest,
        known_resource: bool,
    ) -> impl Future<Output = HandlerResult<Settled<String>>> + Send;
    /// Asks GitHub to update the pull request's branch from its base.
    fn update_branch(
        &mut self,
        request: &UpdateBranchRequest,
        known_resource: bool,
    ) -> impl Future<Output = HandlerResult<Settled<String>>> + Send;
    /// Writes the snapshot through to the projection's row.
    fn upsert_projection(
        &mut self,
        snapshot: &PrRecord,
    ) -> impl Future<Output = HandlerResult<()>> + Send;
    /// Drops the projection's row, journaled under `step`, which names why it goes.
    fn delete_projection(
        &mut self,
        step: &'static str,
        key: &PrKey,
    ) -> impl Future<Output = HandlerResult<()>> + Send;
    /// The merge method the repository's row says to use instead of the configured
    /// preference; see [`repository_merge_method`].
    fn repository_merge_method(
        &mut self,
        repository_id: u64,
    ) -> impl Future<Output = HandlerResult<Option<MergeMethod>>> + Send;
}

struct RestatePullRequest<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    github: &'a GithubApiHandle,
    store: &'a Arc<dyn ProjectionWriter>,
}

impl RestatePullRequest<'_, '_> {
    /// One journaled GitHub step on the pull request: `call` is given the handle and a
    /// clone of `request` on every attempt, and what it settles to comes back.
    async fn github_step<R, T, Fut>(
        &self,
        name: &'static str,
        operation: Operation,
        known_resource: bool,
        request: &R,
        call: impl Fn(GithubApiHandle, R) -> Fut + Clone + Send + Sync + 'static,
    ) -> HandlerResult<Settled<T>>
    where
        R: Clone + Send + Sync + 'static,
        T: Serialize + for<'de> Deserialize<'de> + Send + 'static,
        Fut: Future<Output = Result<T, GithubError>> + Send + 'static,
    {
        let github = self.github.clone();
        let request = request.clone();
        run_github_step(&mut RestateGithubStep {
            ctx: self.ctx,
            name,
            operation,
            known_resource,
            call: move || call(github.clone(), request.clone()),
        })
        .await
    }
}

impl PullRequestEffects for RestatePullRequest<'_, '_> {
    fn key(&self) -> &str {
        self.ctx.key()
    }

    fn can_post_commands(&self) -> bool {
        self.github.can_post_commands()
    }

    async fn now(&mut self, step: &'static str) -> HandlerResult<u64> {
        Ok(self
            .ctx
            .run(|| async { Ok(unix_seconds()) })
            .name(step)
            .await?)
    }

    async fn state(&mut self) -> HandlerResult<Option<PrState>> {
        Ok(self
            .ctx
            .get::<Json<PrState>>(PR_STATE)
            .await?
            .map(Json::into_inner))
    }

    fn set_state(&mut self, state: PrState) {
        self.ctx.set(PR_STATE, Json::from(state));
    }

    fn clear_state(&mut self) {
        self.ctx.clear_all();
    }

    fn schedule_sync(&mut self, request: SyncRequest, after: Option<Duration>) {
        let sync = self
            .ctx
            .object_client::<PullRequestClient>(request_key(&request).to_string())
            .sync(Json::from(request));
        match after {
            Some(after) => sync.send_after(after),
            None => sync.send(),
        };
    }

    async fn fetch_snapshot(
        &mut self,
        request: &SyncRequest,
        known_resource: bool,
    ) -> HandlerResult<Settled<Option<PrRecord>>> {
        self.github_step(
            "fetch-canonical-pr-snapshot",
            Operation::Read,
            known_resource,
            request,
            |github, request| async move { github.fetch_snapshot(&request).await },
        )
        .await
    }

    async fn merge(
        &mut self,
        request: &MergeRequest,
        merge_method: Option<MergeMethod>,
        known_resource: bool,
    ) -> HandlerResult<Settled<Merged>> {
        self.github_step(
            "merge-pull-request",
            Operation::Merge,
            known_resource,
            request,
            move |github, request| async move { github.merge(&request, merge_method).await },
        )
        .await
    }

    async fn post_command(
        &mut self,
        request: &CommandRequest,
        known_resource: bool,
    ) -> HandlerResult<Settled<String>> {
        self.github_step(
            "post-dependabot-command",
            Operation::Comment,
            known_resource,
            request,
            |github, request| async move { github.post_command(&request).await },
        )
        .await
    }

    async fn update_branch(
        &mut self,
        request: &UpdateBranchRequest,
        known_resource: bool,
    ) -> HandlerResult<Settled<String>> {
        self.github_step(
            "update-pull-request-branch",
            Operation::UpdateBranch,
            known_resource,
            request,
            |github, request| async move { github.update_branch(&request).await },
        )
        .await
    }

    async fn upsert_projection(&mut self, snapshot: &PrRecord) -> HandlerResult<()> {
        let store = self.store.clone();
        let snapshot = snapshot.clone();
        self.ctx
            .run_store_step(
                "upsert-pr-projection",
                store_retry_policy(),
                move || async move { store.upsert_pr(&snapshot).await.map_err(store_failure) },
            )
            .await?;
        Ok(())
    }

    async fn delete_projection(&mut self, step: &'static str, key: &PrKey) -> HandlerResult<()> {
        let store = self.store.clone();
        let key = key.clone();
        self.ctx
            .run_store_step(step, store_retry_policy(), move || async move {
                store.delete_pr(&key).await.map_err(store_failure)
            })
            .await?;
        Ok(())
    }

    async fn repository_merge_method(
        &mut self,
        repository_id: u64,
    ) -> HandlerResult<Option<MergeMethod>> {
        let store = self.store.clone();
        Ok(self
            .ctx
            .run_store_step(
                "read-repository-merge-method",
                store_retry_policy(),
                move || async move {
                    repository_merge_method(store.as_ref(), repository_id)
                        .await
                        .map(Json::from)
                        .map_err(store_failure)
                },
            )
            .await?
            .into_inner())
    }
}

/// Debounces, then reads the pull request's canonical snapshot from GitHub and writes it
/// through: the projection's row first, then the object's own state, so a snapshot the
/// object calls its own is one the table already shows. A pull request GitHub no longer
/// serves, or that is not Dependabot's, has its row dropped and its object forgotten.
///
/// Inside the debounce window the first event schedules one trailing sync and sets
/// `sync_pending`; the rest coalesce behind it. The sync that runs clears the flag before
/// it reads GitHub, so an event arriving during the read schedules the next one.
async fn run_sync<E: PullRequestEffects>(
    restate: &mut E,
    request: SyncRequest,
    debounce: Duration,
) -> HandlerResult<SyncOutcome> {
    let now = restate.now("sync-clock").await?;
    let mut state = restate.state().await?.unwrap_or_default();
    if should_debounce_sync(request.bypass_debounce, state.last_synced_at, now, debounce) {
        if !state.sync_pending {
            state.sync_pending = true;
            restate.set_state(state);
            restate.schedule_sync(request, Some(debounce));
        }
        return Ok(SyncOutcome::Debounced);
    }
    state.sync_pending = false;
    restate.set_state(state.clone());

    let fetched = restate
        .fetch_snapshot(&request, state.snapshot.is_some())
        .await?;
    let snapshot = match fetched {
        Settled::Rejected {
            reason: RejectReason::NotFound,
            ..
        } => None,
        fetched => read_result(fetched)?,
    };

    let Some(snapshot) = snapshot else {
        restate
            .delete_projection("delete-ineligible-pr-projection", &request_key(&request))
            .await?;
        restate.clear_state();
        return Ok(SyncOutcome::Deleted);
    };
    restate.upsert_projection(&snapshot).await?;
    state.snapshot = Some(snapshot.clone());
    state.last_synced_at = Some(snapshot.synced_at);
    state.push_history(ActionLog {
        at: snapshot.synced_at,
        action: "sync".to_owned(),
        detail: format!("canonical snapshot at {}", short_sha(&snapshot.head_sha)),
    });
    if let Some(completion_id) = request.completion_id {
        state.complete_sync(completion_id);
    }
    restate.set_state(state);
    Ok(SyncOutcome::Synced {
        head_sha: snapshot.head_sha,
    })
}

/// Retires the pull request's row and state together, unless a sweep's fence finds it
/// synced since the sweep began, in which case neither is touched. The row goes first: a
/// store that refuses leaves the state for the retry.
async fn run_closed<E: PullRequestEffects>(
    restate: &mut E,
    request: ClosedRequest,
) -> HandlerResult<ClosedOutcome> {
    let key = restate
        .key()
        .parse::<PrKey>()
        .map_err(|error| TerminalError::new(error.to_string()))?;
    let state = restate.state().await?;
    if !closed_retires(state.as_ref(), request) {
        return Ok(ClosedOutcome::Kept);
    }
    restate
        .delete_projection("delete-closed-pr-projection", &key)
        .await?;
    restate.clear_state();
    Ok(ClosedOutcome::Retired)
}

/// Guards the target against the snapshot, then merges with the method the repository's
/// row resolved. A merge that landed drops the row and forgets the snapshot, keeping the
/// history; one GitHub rejected keeps both; a fatal answer fails the handler with the state
/// untouched. A target the guard rejects reaches neither GitHub nor the store.
async fn run_merge<E: PullRequestEffects>(
    restate: &mut E,
    request: MergeRequest,
) -> HandlerResult<ActionOutcome> {
    let mut state = restate.state().await?.unwrap_or_default();
    if let Err(reason) = guard_target(state.snapshot.as_ref(), &request.target) {
        return Ok(rejected(reason));
    }

    let merge_method = restate
        .repository_merge_method(request.target.repository_id)
        .await?;
    let result = restate
        .merge(&request, merge_method, state.snapshot.is_some())
        .await?;
    let outcome = merge_result(result)?;
    if matches!(outcome, ActionOutcome::Succeeded { .. }) {
        restate
            .delete_projection("delete-merged-pr-projection", &request.target.key())
            .await?;
        state.snapshot = None;
    }
    let log_at = restate.now("merge-log-clock").await?;
    state.push_history(ActionLog {
        at: log_at,
        action: "merge".to_owned(),
        detail: outcome_detail(&outcome),
    });
    restate.set_state(state);
    Ok(outcome)
}

/// Guards the command — for a user identity first, then the target — and posts it,
/// logging the outcome under the command's own name.
async fn run_command<E: PullRequestEffects>(
    restate: &mut E,
    request: CommandRequest,
) -> HandlerResult<ActionOutcome> {
    let mut state = restate.state().await?.unwrap_or_default();
    if let Err(reason) = guard_command(
        restate.can_post_commands(),
        state.snapshot.as_ref(),
        &request.target,
    ) {
        return Ok(rejected(reason));
    }

    let result = restate
        .post_command(&request, state.snapshot.is_some())
        .await?;
    let outcome = action_result(result)?;
    let log_at = restate.now("command-log-clock").await?;
    state.push_history(ActionLog {
        at: log_at,
        action: request.command.to_string(),
        detail: outcome_detail(&outcome),
    });
    restate.set_state(state);
    Ok(outcome)
}

/// Guards the target against the snapshot, then asks GitHub to update the branch. An
/// update that landed asks for a follow-up sync that honours the debounce, so the row
/// catches up before the webhook does without cutting ahead of a storm's coalescing.
async fn run_update_branch<E: PullRequestEffects>(
    restate: &mut E,
    request: UpdateBranchRequest,
) -> HandlerResult<ActionOutcome> {
    let mut state = restate.state().await?.unwrap_or_default();
    if let Err(reason) = guard_target(state.snapshot.as_ref(), &request.target) {
        return Ok(rejected(reason));
    }

    let result = restate
        .update_branch(&request, state.snapshot.is_some())
        .await?;
    let outcome = action_result(result)?;
    let log_at = restate.now("update-branch-log-clock").await?;
    state.push_history(ActionLog {
        at: log_at,
        action: "update_branch".to_owned(),
        detail: outcome_detail(&outcome),
    });
    restate.set_state(state);
    if matches!(outcome, ActionOutcome::Succeeded { .. }) {
        restate.schedule_sync(
            SyncRequest {
                repository_id: request.target.repository_id,
                owner: request.target.owner,
                repo: request.target.repo,
                number: request.target.number,
                bypass_debounce: false,
                completion_id: None,
            },
            None,
        );
    }
    Ok(outcome)
}

impl PullRequest {
    fn restate<'a, 'ctx>(&'a self, ctx: &'a ObjectContext<'ctx>) -> RestatePullRequest<'a, 'ctx> {
        RestatePullRequest {
            ctx,
            github: &self.github,
            store: &self.store,
        }
    }
}

#[restate_sdk::object]
impl PullRequest {
    #[handler(ingress_private)]
    async fn sync(&self, ctx: ObjectContext<'_>, request: Json<SyncRequest>) -> HandlerResult<()> {
        traced(
            "PullRequest/sync",
            ctx.key(),
            run_sync(&mut self.restate(&ctx), request.into_inner(), self.debounce),
        )
        .await
        .map(|_| ())
    }

    #[handler(ingress_private)]
    async fn closed(&self, ctx: ObjectContext<'_>, request: ClosedRequest) -> HandlerResult<()> {
        traced(
            "PullRequest/closed",
            ctx.key(),
            run_closed(&mut self.restate(&ctx), request),
        )
        .await
        .map(|_| ())
    }

    #[handler(ingress_private)]
    async fn merge(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<MergeRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        traced(
            "PullRequest/merge",
            ctx.key(),
            run_merge(&mut self.restate(&ctx), request.into_inner()),
        )
        .await
        .map(Json::from)
    }

    #[handler(ingress_private)]
    async fn command(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<CommandRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        traced(
            "PullRequest/command",
            ctx.key(),
            run_command(&mut self.restate(&ctx), request.into_inner()),
        )
        .await
        .map(Json::from)
    }

    #[handler(ingress_private)]
    async fn update_branch(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<UpdateBranchRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        traced(
            "PullRequest/update_branch",
            ctx.key(),
            run_update_branch(&mut self.restate(&ctx), request.into_inner()),
        )
        .await
        .map(Json::from)
    }

    #[handler]
    async fn status(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<Option<PrState>>> {
        traced_read("PullRequest/status", ctx.key(), async {
            Ok(Json::from(
                ctx.get::<Json<PrState>>(PR_STATE)
                    .await?
                    .map(Json::into_inner),
            ))
        })
        .await
    }
}

fn should_debounce_sync(
    bypass_debounce: bool,
    last_synced_at: Option<u64>,
    now: u64,
    debounce: Duration,
) -> bool {
    !bypass_debounce
        && last_synced_at.is_some_and(|last| now.saturating_sub(last) < debounce.as_secs())
}

/// Whether `closed` retires the pull request, or a sweep's fence finds it synced since the
/// sweep's listing started — reopened and re-projected — and leaves it alone.
///
/// The boundary mirrors the projection's `synced_at < reconcile_start` prune guard: a
/// pull request synced at or after that instant would have kept its row too.
fn closed_retires(state: Option<&PrState>, request: ClosedRequest) -> bool {
    let Some(synced_before) = request.synced_before else {
        return true;
    };
    let last_synced_at = state.and_then(|state| state.last_synced_at);
    !last_synced_at.is_some_and(|last| last >= synced_before)
}

/// The merge method a repository's row says to use instead of the configured preference.
///
/// The row carries the method its last sync resolved when the repository disallows the
/// preference. A missing row (the repository was purged mid-batch) or a row from before
/// the column reads as no override, and the client falls back to the preference.
async fn repository_merge_method(
    store: &dyn ProjectionWriter,
    repository_id: u64,
) -> Result<Option<MergeMethod>, StoreError> {
    Ok(store
        .get_repo(repository_id)
        .await?
        .and_then(|repository| repository.merge_method))
}

fn outcome_detail(outcome: &ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Succeeded { detail, .. } => detail.clone(),
        ActionOutcome::Rejected { reason } => reason.to_string(),
    }
}

/// The key of the `PullRequest` object a sync request addresses.
pub(crate) fn request_key(request: &SyncRequest) -> PrKey {
    PrKey::new(request.repository_id, request.number)
}

/// Tells the pull request's object it is no longer open, one-way, from any Restate context.
/// A sweep's drain passes the fence the prune recorded — the instant its listing started —
/// as `request.synced_before`; a webhook passes the default. The projection row is usually
/// already gone by the time this is sent; `closed` tolerates that and retires the durable
/// state regardless, so the object stops serving a snapshot nobody else has.
pub(crate) fn close_pull_request<'ctx>(
    ctx: &impl ContextClient<'ctx>,
    key: &PrKey,
    request: ClosedRequest,
) {
    ctx.object_client::<PullRequestClient>(key.to_string())
        .closed(request)
        .send();
}

/// Checks a mutation's target against the object's canonical snapshot before anything is
/// sent to GitHub.
///
/// The dashboard acts on what it last saw; the snapshot is what the object knows now. With
/// no snapshot, or a snapshot of a different pull request than the target names, there is
/// nothing to act on. A target whose expected head has since moved is stale, and the
/// rejection names both SHAs so the dashboard can show what changed.
///
/// Past the guard the pull request is a known resource — the object read it from GitHub to
/// get the snapshot — so the mutation's GitHub step is told as much, exactly as `sync`
/// tells its read. A 404 on the client's verify read then means the pull request has gone
/// since the table showed it, and the target is rejected as not found rather than failed.
fn guard_target(snapshot: Option<&PrRecord>, target: &PrTarget) -> Result<(), RejectReason> {
    let snapshot = snapshot
        .filter(|snapshot| target_matches_snapshot(target, snapshot))
        .ok_or(RejectReason::NotFound)?;
    if snapshot.head_sha != target.expected_sha {
        return Err(RejectReason::StaleSha {
            expected: target.expected_sha.clone(),
            actual: snapshot.head_sha.clone(),
        });
    }
    Ok(())
}

fn target_matches_snapshot(target: &PrTarget, snapshot: &PrRecord) -> bool {
    target.repository_id == snapshot.repository_id
        && target.owner == snapshot.owner
        && target.repo == snapshot.repo
        && target.number == snapshot.number
}

/// [`guard_target`] for a `@dependabot` command, which also needs a user identity to
/// be posted under. A deployment without one — no `GITHUB_USER_PAT` — has every
/// command rejected before its target is judged: the want of a token is the
/// deployment's condition, not the pull request's, and the reason the operator can
/// act on. Nothing is sent to GitHub either way.
fn guard_command(
    can_post_commands: bool,
    snapshot: Option<&PrRecord>,
    target: &PrTarget,
) -> Result<(), RejectReason> {
    if !can_post_commands {
        return Err(RejectReason::NoUserToken);
    }
    guard_target(snapshot, target)
}

pub(crate) fn short_sha(value: &str) -> &str {
    value.get(..7).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use dependaboard_core::{DependabotCommand, GithubErrorResponse, RepoRecord, UserId};
    use restate_sdk::service::Discoverable;

    use super::*;
    use crate::{
        handler::RetryableServiceError,
        test_support::{MemoryPrStore, repository, snapshot, target},
    };

    /// The fake clock's first reading, in Unix seconds.
    const CLOCK_EPOCH: u64 = 1_700_000_000;
    /// The event debounce the object under test runs with.
    const DEBOUNCE: Duration = Duration::from_secs(20);
    /// The commit every merge in these tests makes.
    const MERGE_SHA: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6";

    /// A sync of [`target`]'s pull request, as a webhook would ask for it.
    fn sync_request() -> SyncRequest {
        SyncRequest {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            bypass_debounce: false,
            completion_id: None,
        }
    }

    fn merge_request() -> MergeRequest {
        MergeRequest {
            batch_id: "batch-1".to_owned(),
            target: target(),
        }
    }

    fn update_branch_request() -> UpdateBranchRequest {
        UpdateBranchRequest {
            batch_id: "batch-1".to_owned(),
            target: target(),
        }
    }

    fn command_request() -> CommandRequest {
        CommandRequest {
            batch_id: "batch-1".to_owned(),
            target: target(),
            user_id: UserId::new("dashboard"),
            command: DependabotCommand::Rebase,
        }
    }

    /// The state of an object that synced [`snapshot`] when the fixture says it did, long
    /// before the fake clock's epoch.
    fn synced() -> PrState {
        PrState {
            snapshot: Some(snapshot()),
            last_synced_at: Some(snapshot().synced_at),
            ..PrState::default()
        }
    }

    fn not_found() -> GithubErrorResponse {
        GithubErrorResponse {
            status: 404,
            message: "Not Found".to_owned(),
            ..Default::default()
        }
    }

    fn not_mergeable() -> GithubErrorResponse {
        GithubErrorResponse {
            status: 405,
            message: "Pull Request is not mergeable".to_owned(),
            ..Default::default()
        }
    }

    fn github_500() -> GithubErrorResponse {
        GithubErrorResponse {
            status: 500,
            message: "Internal Server Error".to_owned(),
            ..Default::default()
        }
    }

    /// One answer from GitHub, as the step that asked for it settles.
    #[derive(Debug)]
    enum Answer {
        Snapshot(Settled<Option<PrRecord>>),
        Merge(Settled<Merged>),
        /// A comment or a branch update; both settle to GitHub's message.
        Action(Settled<String>),
    }

    /// Stands in for Restate, GitHub and the store behind one `PullRequest` object and
    /// records what its handlers asked of them.
    ///
    /// GitHub answers from a scripted queue in the order asked; a handler that asks more
    /// than the test scripted, or for something else, fails the test. The store is a real
    /// in-memory projection, so a row is dropped or kept as the schema would have it. The
    /// clock advances by a second per reading, so no two readings coincide.
    #[derive(Default)]
    struct RecordedPullRequest {
        store: MemoryPrStore,
        /// The object's durable state, as Restate holds it.
        state: Option<PrState>,
        can_post_commands: bool,
        answers: VecDeque<Answer>,
        /// Every GitHub step taken, with whether the pull request was a known resource.
        asked: Vec<(Operation, bool)>,
        /// Every one-way sync sent, with its delay.
        scheduled: Vec<(SyncRequest, Option<Duration>)>,
        /// Every store step taken, by the name its journal entry would carry.
        store_steps: Vec<&'static str>,
        /// Seconds the clock has been read for, from a fixed epoch.
        clock_readings: u64,
        /// The state the object held when GitHub was first asked, if it was.
        held_when_asked: Option<PrState>,
        /// The state the object held when the projection's row was written, if it was.
        held_when_projected: Option<PrState>,
        /// A failure the next store step ends with instead of running, taken once.
        store_failure: Option<HandlerError>,
    }

    /// An object whose repository the projection lists, holding `state` and, when that
    /// has a snapshot, the snapshot's row.
    async fn pull_request(state: Option<PrState>) -> RecordedPullRequest {
        let store = MemoryPrStore::default();
        store.upsert_repo(&repository()).await.unwrap();
        if let Some(snapshot) = state.as_ref().and_then(|state| state.snapshot.as_ref()) {
            store.upsert_pr(snapshot).await.unwrap();
        }
        RecordedPullRequest {
            store,
            state,
            ..Default::default()
        }
    }

    impl RecordedPullRequest {
        fn answering(mut self, answers: impl IntoIterator<Item = Answer>) -> Self {
            self.answers = answers.into_iter().collect();
            self
        }

        /// Lets `duration` pass on the fake clock.
        fn wait(&mut self, duration: Duration) {
            self.clock_readings += duration.as_secs();
        }

        /// What the object holds: its state, or a fresh object's.
        fn held(&self) -> PrState {
            self.state.clone().unwrap_or_default()
        }

        /// The projection's row for the pull request, if it has one.
        async fn row(&self) -> Option<PrRecord> {
            self.store.get_pr(&target().key()).await.unwrap()
        }

        fn answer(&mut self, operation: Operation, known_resource: bool) -> Answer {
            if self.held_when_asked.is_none() {
                self.held_when_asked = Some(self.held());
            }
            self.asked.push((operation, known_resource));
            self.answers
                .pop_front()
                .unwrap_or_else(|| panic!("the handler asked GitHub to {operation} unscripted"))
        }

        /// Records the store step about to run, and ends it with the failure scripted for
        /// the next one instead, if there is one; taken once.
        fn store_step(&mut self, step: &'static str) -> HandlerResult<()> {
            self.store_steps.push(step);
            self.store_failure.take().map_or(Ok(()), Err)
        }
    }

    impl PullRequestEffects for RecordedPullRequest {
        fn key(&self) -> &str {
            "7#9"
        }

        fn can_post_commands(&self) -> bool {
            self.can_post_commands
        }

        async fn now(&mut self, _step: &'static str) -> HandlerResult<u64> {
            let reading = CLOCK_EPOCH + self.clock_readings;
            self.clock_readings += 1;
            Ok(reading)
        }

        async fn state(&mut self) -> HandlerResult<Option<PrState>> {
            Ok(self.state.clone())
        }

        fn set_state(&mut self, state: PrState) {
            self.state = Some(state);
        }

        fn clear_state(&mut self) {
            self.state = None;
        }

        fn schedule_sync(&mut self, request: SyncRequest, after: Option<Duration>) {
            self.scheduled.push((request, after));
        }

        async fn fetch_snapshot(
            &mut self,
            _request: &SyncRequest,
            known_resource: bool,
        ) -> HandlerResult<Settled<Option<PrRecord>>> {
            match self.answer(Operation::Read, known_resource) {
                Answer::Snapshot(settled) => Ok(settled),
                other => panic!("the handler asked GitHub for a snapshot; scripted {other:?}"),
            }
        }

        async fn merge(
            &mut self,
            _request: &MergeRequest,
            _merge_method: Option<MergeMethod>,
            known_resource: bool,
        ) -> HandlerResult<Settled<Merged>> {
            match self.answer(Operation::Merge, known_resource) {
                Answer::Merge(settled) => Ok(settled),
                other => panic!("the handler asked GitHub to merge; scripted {other:?}"),
            }
        }

        async fn post_command(
            &mut self,
            _request: &CommandRequest,
            known_resource: bool,
        ) -> HandlerResult<Settled<String>> {
            match self.answer(Operation::Comment, known_resource) {
                Answer::Action(settled) => Ok(settled),
                other => panic!("the handler asked GitHub to comment; scripted {other:?}"),
            }
        }

        async fn update_branch(
            &mut self,
            _request: &UpdateBranchRequest,
            known_resource: bool,
        ) -> HandlerResult<Settled<String>> {
            match self.answer(Operation::UpdateBranch, known_resource) {
                Answer::Action(settled) => Ok(settled),
                other => {
                    panic!("the handler asked GitHub to update the branch; scripted {other:?}")
                }
            }
        }

        async fn upsert_projection(&mut self, snapshot: &PrRecord) -> HandlerResult<()> {
            self.held_when_projected = Some(self.held());
            self.store_step("upsert-pr-projection")?;
            self.store.upsert_pr(snapshot).await.map_err(store_failure)
        }

        async fn delete_projection(
            &mut self,
            step: &'static str,
            key: &PrKey,
        ) -> HandlerResult<()> {
            self.store_step(step)?;
            self.store.delete_pr(key).await.map_err(store_failure)
        }

        async fn repository_merge_method(
            &mut self,
            repository_id: u64,
        ) -> HandlerResult<Option<MergeMethod>> {
            self.store_step("read-repository-merge-method")?;
            repository_merge_method(&self.store, repository_id)
                .await
                .map_err(store_failure)
        }
    }

    #[tokio::test]
    async fn a_second_sync_inside_the_debounce_window_is_coalesced_behind_the_one_already_scheduled()
     {
        let just_synced = PrState {
            last_synced_at: Some(CLOCK_EPOCH),
            ..synced()
        };
        let mut restate = pull_request(Some(just_synced))
            .await
            .answering([Answer::Snapshot(Settled::Ok(Some(snapshot())))]);

        let first = run_sync(&mut restate, sync_request(), DEBOUNCE)
            .await
            .unwrap();
        let second = run_sync(&mut restate, sync_request(), DEBOUNCE)
            .await
            .unwrap();

        assert_eq!(
            (first, second),
            (SyncOutcome::Debounced, SyncOutcome::Debounced)
        );
        assert_eq!(
            restate.scheduled,
            vec![(sync_request(), Some(DEBOUNCE))],
            "one trailing sync for the storm, not one per event"
        );
        assert!(restate.held().sync_pending);
        assert!(restate.asked.is_empty(), "neither event reached GitHub");

        // The trailing sync arrives once the window has passed.
        restate.wait(DEBOUNCE);
        let trailing = run_sync(&mut restate, sync_request(), DEBOUNCE)
            .await
            .unwrap();

        assert!(matches!(trailing, SyncOutcome::Synced { .. }));
        assert_eq!(restate.asked, vec![(Operation::Read, true)]);
        assert!(!restate.held().sync_pending);
        assert_eq!(
            restate.scheduled.len(),
            1,
            "it schedules no successor of its own"
        );
    }

    #[tokio::test]
    async fn the_trailing_sync_clears_its_pending_flag_before_it_reads_github() {
        let awaiting_trailing_sync = PrState {
            sync_pending: true,
            ..synced()
        };
        let mut restate = pull_request(Some(awaiting_trailing_sync))
            .await
            .answering([Answer::Snapshot(Settled::Ok(Some(snapshot())))]);

        run_sync(&mut restate, sync_request(), DEBOUNCE)
            .await
            .unwrap();

        assert!(
            !restate
                .held_when_asked
                .as_ref()
                .expect("GitHub was read")
                .sync_pending,
            "an event arriving during the read finds the flag down and schedules the next \
             trailing sync, rather than being coalesced behind this one"
        );
        assert!(!restate.held().sync_pending);
        assert!(restate.scheduled.is_empty());
    }

    #[tokio::test]
    async fn a_sync_writes_the_projection_before_it_records_the_snapshot_as_its_own() {
        let pushed = PrRecord {
            head_sha: "def456".to_owned(),
            synced_at: CLOCK_EPOCH,
            ..snapshot()
        };
        let mut restate = pull_request(Some(synced()))
            .await
            .answering([Answer::Snapshot(Settled::Ok(Some(pushed.clone())))]);
        let refresh = SyncRequest {
            completion_id: Some("refresh-1".to_owned()),
            ..sync_request()
        };

        let outcome = run_sync(&mut restate, refresh, DEBOUNCE).await.unwrap();

        assert_eq!(
            outcome,
            SyncOutcome::Synced {
                head_sha: "def456".to_owned()
            }
        );
        assert_eq!(
            restate.asked,
            vec![(Operation::Read, true)],
            "the object had a snapshot, so the read is of a known resource"
        );
        assert_eq!(
            restate
                .held_when_projected
                .as_ref()
                .expect("the row was written")
                .snapshot,
            Some(snapshot()),
            "the row goes first: a snapshot the object calls its own is one the table shows"
        );
        assert_eq!(restate.row().await, Some(pushed.clone()));
        let state = restate.state.expect("the object keeps its state");
        assert_eq!(state.snapshot, Some(pushed));
        assert_eq!(state.last_synced_at, Some(CLOCK_EPOCH));
        assert_eq!(
            state
                .history
                .last()
                .map(|log| (log.at, log.action.as_str(), log.detail.as_str())),
            Some((CLOCK_EPOCH, "sync", "canonical snapshot at def456"))
        );
        assert_eq!(
            state.completed_sync_ids,
            vec!["refresh-1".to_owned()],
            "the dashboard polls for the refresh it asked for by this id"
        );
    }

    /// GitHub says so two ways: a 404 on a pull request the object has read before, and an
    /// open pull request that is not Dependabot's. Either way the table must stop listing
    /// it and the object must stop serving a snapshot nobody else has.
    #[tokio::test]
    async fn a_sync_that_finds_no_open_dependabot_pull_request_drops_the_row_and_forgets_the_object()
     {
        let gone = Answer::Snapshot(Settled::Rejected {
            reason: RejectReason::NotFound,
            response: not_found(),
        });
        let not_dependabots = Answer::Snapshot(Settled::Ok(None));

        for (says, answer) in [
            ("a 404 on a pull request read before", gone),
            (
                "an open pull request that is not Dependabot's",
                not_dependabots,
            ),
        ] {
            let mut restate = pull_request(Some(synced())).await.answering([answer]);

            let outcome = run_sync(&mut restate, sync_request(), DEBOUNCE)
                .await
                .unwrap();

            assert_eq!(outcome, SyncOutcome::Deleted, "{says}");
            assert_eq!(restate.row().await, None, "{says}");
            assert_eq!(
                restate.state, None,
                "{says}: the next handler finds a fresh object"
            );
        }
    }

    #[tokio::test]
    async fn a_merge_github_rejected_keeps_the_snapshot() {
        let mut restate =
            pull_request(Some(synced()))
                .await
                .answering([Answer::Merge(Settled::Rejected {
                    reason: RejectReason::NotMergeable,
                    response: not_mergeable(),
                })]);

        let outcome = run_merge(&mut restate, merge_request()).await.unwrap();

        assert_eq!(
            outcome,
            ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable
            }
        );
        assert_eq!(
            restate.row().await,
            Some(snapshot()),
            "the pull request is still open; the table keeps listing it"
        );
        let state = restate.state.expect("the object keeps its state");
        assert_eq!(
            state.snapshot,
            Some(snapshot()),
            "there is still something to act on"
        );
        assert_eq!(
            state
                .history
                .last()
                .map(|log| (log.action.as_str(), log.detail.as_str())),
            Some(("merge", "GitHub reports this pull request is not mergeable"))
        );
    }

    #[tokio::test]
    async fn a_fatal_github_answer_fails_the_handler_with_the_state_untouched() {
        let mut restate =
            pull_request(Some(synced()))
                .await
                .answering([Answer::Merge(Settled::Fatal {
                    response: github_500(),
                })]);

        let error = run_merge(&mut restate, merge_request()).await.unwrap_err();

        let cause: &dyn std::error::Error = error.as_ref();
        assert_eq!(
            cause.to_string(),
            "Terminal error [500]: GitHub mutation failed with HTTP 500: Internal Server Error"
        );
        assert_eq!(
            restate.state,
            Some(synced()),
            "not even a history line: the attempt is the batch's to record as failed"
        );
        assert_eq!(restate.store_steps, vec!["read-repository-merge-method"]);
        assert_eq!(restate.row().await, Some(snapshot()));
    }

    #[tokio::test]
    async fn a_target_the_guard_rejects_reaches_neither_github_nor_the_store() {
        let mut restate = pull_request(Some(synced())).await;
        restate.can_post_commands = true;
        let moved = PrTarget {
            expected_sha: "def456".to_owned(),
            ..target()
        };
        let stale = ActionOutcome::Rejected {
            reason: RejectReason::StaleSha {
                expected: "def456".to_owned(),
                actual: "abc123".to_owned(),
            },
        };

        let merge = run_merge(
            &mut restate,
            MergeRequest {
                target: moved.clone(),
                ..merge_request()
            },
        )
        .await
        .unwrap();
        let command = run_command(
            &mut restate,
            CommandRequest {
                target: moved.clone(),
                ..command_request()
            },
        )
        .await
        .unwrap();
        let update_branch = run_update_branch(
            &mut restate,
            UpdateBranchRequest {
                target: moved,
                ..update_branch_request()
            },
        )
        .await
        .unwrap();

        assert_eq!((&merge, &command, &update_branch), (&stale, &stale, &stale));
        assert!(restate.asked.is_empty(), "nothing was sent to GitHub");
        assert!(
            restate.store_steps.is_empty(),
            "not even the merge method was read"
        );
        assert!(restate.scheduled.is_empty());
        assert_eq!(restate.row().await, Some(snapshot()));
        assert_eq!(
            restate.state,
            Some(synced()),
            "not even a history line: the rejection is the batch's to record"
        );
    }

    #[tokio::test]
    async fn a_command_without_a_user_token_is_rejected_before_github_is_asked() {
        let mut restate = pull_request(Some(synced())).await;

        let outcome = run_command(&mut restate, command_request()).await.unwrap();

        assert_eq!(
            outcome,
            ActionOutcome::Rejected {
                reason: RejectReason::NoUserToken
            }
        );
        assert!(restate.asked.is_empty());
        assert_eq!(restate.state, Some(synced()));
    }

    #[tokio::test]
    async fn a_command_is_posted_and_logged_under_its_own_name() {
        let mut restate = pull_request(Some(synced()))
            .await
            .answering([Answer::Action(Settled::Ok(
                "@dependabot rebase posted".to_owned(),
            ))]);
        restate.can_post_commands = true;

        let outcome = run_command(&mut restate, command_request()).await.unwrap();

        assert_eq!(
            outcome,
            ActionOutcome::Succeeded {
                detail: "@dependabot rebase posted".to_owned(),
                merge_sha: None,
            }
        );
        assert_eq!(restate.asked, vec![(Operation::Comment, true)]);
        let state = restate.state.expect("the object keeps its state");
        assert_eq!(
            state.snapshot,
            Some(snapshot()),
            "Dependabot rebases in its own time; the snapshot stands until the webhook"
        );
        assert_eq!(
            state
                .history
                .last()
                .map(|log| (log.action.as_str(), log.detail.as_str())),
            Some(("rebase", "@dependabot rebase posted"))
        );
    }

    #[tokio::test]
    async fn a_branch_update_that_landed_asks_for_a_sync_that_honours_the_debounce() {
        let mut restate = pull_request(Some(synced()))
            .await
            .answering([Answer::Action(Settled::Ok(
                "Updating pull request branch.".to_owned(),
            ))]);

        let outcome = run_update_branch(&mut restate, update_branch_request())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ActionOutcome::Succeeded {
                detail: "Updating pull request branch.".to_owned(),
                merge_sha: None,
            }
        );
        assert_eq!(restate.asked, vec![(Operation::UpdateBranch, true)]);
        assert_eq!(
            restate.scheduled,
            vec![(sync_request(), None)],
            "the row catches up before the webhook does, but the sync queues behind a storm's \
             coalescing like any event's, and nobody is polling for it"
        );
        let state = restate.state.expect("the object keeps its state");
        assert_eq!(
            state.snapshot,
            Some(snapshot()),
            "GitHub finishes the update after it has answered; the sync brings the new head"
        );
        assert_eq!(
            state
                .history
                .last()
                .map(|log| (log.action.as_str(), log.detail.as_str())),
            Some(("update_branch", "Updating pull request branch."))
        );
    }

    #[tokio::test]
    async fn a_branch_update_github_rejected_asks_for_no_sync() {
        let mut restate = pull_request(Some(synced()))
            .await
            .answering([Answer::Action(Settled::Rejected {
                reason: RejectReason::Forbidden,
                response: GithubErrorResponse {
                    status: 403,
                    message: "Resource not accessible by integration".to_owned(),
                    ..Default::default()
                },
            })]);

        let outcome = run_update_branch(&mut restate, update_branch_request())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden
            }
        );
        assert!(
            restate.scheduled.is_empty(),
            "nothing changed on GitHub for a sync to catch up with"
        );
        assert_eq!(
            restate.held().history.last().map(|log| log.detail.as_str()),
            Some("the configured identity is not allowed to perform this action")
        );
    }

    #[tokio::test]
    async fn closed_under_a_sweeps_fence_keeps_a_pull_request_synced_since() {
        let resynced = PrState {
            last_synced_at: Some(1_000),
            ..synced()
        };
        let mut restate = pull_request(Some(resynced.clone())).await;

        let outcome = run_closed(
            &mut restate,
            ClosedRequest {
                synced_before: Some(900),
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome, ClosedOutcome::Kept);
        assert_eq!(
            restate.row().await,
            Some(snapshot()),
            "a webhook re-projected it after the sweep listed: it is open again"
        );
        assert_eq!(restate.state, Some(resynced));
    }

    #[tokio::test]
    async fn a_webhooks_closed_retires_the_row_and_the_state_together() {
        let mut restate = pull_request(Some(synced())).await;

        let outcome = run_closed(&mut restate, ClosedRequest::default())
            .await
            .unwrap();

        assert_eq!(outcome, ClosedOutcome::Retired);
        assert_eq!(restate.store_steps, vec!["delete-closed-pr-projection"]);
        assert_eq!(restate.row().await, None);
        assert_eq!(restate.state, None);
        assert!(restate.asked.is_empty(), "GitHub already said so");
    }

    #[tokio::test]
    async fn a_store_that_refuses_the_delete_leaves_the_state_for_the_retry() {
        let mut restate = pull_request(Some(synced())).await;
        restate.store_failure =
            Some(RetryableServiceError::Store("database is locked".to_owned()).into());

        let outcome = run_closed(&mut restate, ClosedRequest::default()).await;

        assert!(outcome.is_err());
        assert_eq!(restate.row().await, Some(snapshot()));
        assert_eq!(
            restate.state,
            Some(synced()),
            "row and state go together or not at all: a state cleared ahead of a row that \
             stayed would leave the table listing a pull request no object answers for"
        );
    }

    #[tokio::test]
    async fn a_merge_that_landed_drops_the_row_and_keeps_its_history_without_a_snapshot() {
        let mut restate =
            pull_request(Some(synced()))
                .await
                .answering([Answer::Merge(Settled::Ok(Merged {
                    detail: "merged".to_owned(),
                    sha: Some(MERGE_SHA.to_owned()),
                }))]);

        let outcome = run_merge(&mut restate, merge_request()).await.unwrap();

        assert_eq!(
            outcome,
            ActionOutcome::Succeeded {
                detail: "merged".to_owned(),
                merge_sha: Some(MERGE_SHA.to_owned()),
            }
        );
        assert_eq!(restate.asked, vec![(Operation::Merge, true)]);
        assert_eq!(
            restate.row().await,
            None,
            "the table no longer lists a merged pull request"
        );
        let state = restate.state.expect("the object keeps its history");
        assert_eq!(state.snapshot, None, "there is nothing left to act on");
        assert_eq!(
            state
                .history
                .last()
                .map(|log| (log.action.as_str(), log.detail.as_str())),
            Some(("merge", "merged"))
        );
    }

    #[test]
    fn pull_request_exposes_only_status_through_ingress() {
        let discovery = <PullRequest as Discoverable>::discover();
        assert_ne!(discovery.ingress_private, Some(true));

        for handler_name in ["sync", "closed", "merge", "command", "update_branch"] {
            let handler = discovery
                .handlers
                .iter()
                .find(|handler| handler.name.as_str() == handler_name)
                .unwrap_or_else(|| panic!("missing PullRequest/{handler_name}"));
            assert_eq!(handler.ingress_private, Some(true));
        }

        let status = discovery
            .handlers
            .iter()
            .find(|handler| handler.name.as_str() == "status")
            .expect("missing PullRequest/status");
        assert_ne!(status.ingress_private, Some(true));
    }

    #[test]
    fn a_target_taken_from_the_current_snapshot_passes_the_guard() {
        assert_eq!(guard_target(Some(&snapshot()), &target()), Ok(()));
    }

    #[test]
    fn a_target_the_object_holds_no_snapshot_for_is_not_found() {
        assert_eq!(
            guard_target(None, &target()),
            Err(RejectReason::NotFound),
            "nothing has been synced yet: there is nothing to act on"
        );
        let mut other_repository = target();
        other_repository.repo = "other".to_owned();
        assert_eq!(
            guard_target(Some(&snapshot()), &other_repository),
            Err(RejectReason::NotFound),
            "the snapshot belongs to a different pull request than the target names"
        );
    }

    #[test]
    fn a_target_whose_head_has_moved_is_stale_and_names_both_shas() {
        let mut before_a_push = target();
        before_a_push.expected_sha = "def456".to_owned();

        assert_eq!(
            guard_target(Some(&snapshot()), &before_a_push),
            Err(RejectReason::StaleSha {
                expected: "def456".to_owned(),
                actual: "abc123".to_owned(),
            })
        );
    }

    /// A deployment without a user PAT cannot post `@dependabot` commands. A command is
    /// rejected for that first, whatever the target: the want of a token is the
    /// deployment's condition, not the pull request's, and it is the reason the
    /// operator can act on. With a token, the target guard has its say as usual.
    #[test]
    fn a_command_is_rejected_for_want_of_a_user_token_before_its_target_is_judged() {
        let mut before_a_push = target();
        before_a_push.expected_sha = "def456".to_owned();

        assert_eq!(
            guard_command(false, Some(&snapshot()), &target()),
            Err(RejectReason::NoUserToken)
        );
        assert_eq!(
            guard_command(false, Some(&snapshot()), &before_a_push),
            Err(RejectReason::NoUserToken),
            "the missing token outranks a stale head"
        );
        assert_eq!(
            guard_command(false, None, &target()),
            Err(RejectReason::NoUserToken),
            "and a pull request the object holds no snapshot for"
        );

        assert_eq!(guard_command(true, Some(&snapshot()), &target()), Ok(()));
        assert_eq!(
            guard_command(true, Some(&snapshot()), &before_a_push),
            Err(RejectReason::StaleSha {
                expected: "def456".to_owned(),
                actual: "abc123".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn a_merge_uses_the_method_its_repository_resolved_at_its_last_sync() {
        let store = MemoryPrStore::default();
        store
            .upsert_repo(&RepoRecord {
                merge_method: Some(MergeMethod::Rebase),
                ..repository()
            })
            .await
            .unwrap();
        store
            .upsert_repo(&RepoRecord {
                repository_id: 8,
                ..repository()
            })
            .await
            .unwrap();

        assert_eq!(
            repository_merge_method(&store, 7).await.unwrap(),
            Some(MergeMethod::Rebase),
            "the repository disallows the configured preference"
        );
        assert_eq!(
            repository_merge_method(&store, 8).await.unwrap(),
            None,
            "the preference is allowed there, or the row predates the column"
        );
        assert_eq!(
            repository_merge_method(&store, 9).await.unwrap(),
            None,
            "a repository purged mid-batch has no row; the merge falls back to the preference"
        );
    }

    #[test]
    fn a_manual_sync_bypasses_the_event_debounce() {
        let debounce = Duration::from_secs(20);
        assert!(should_debounce_sync(false, Some(100), 101, debounce));
        assert!(!should_debounce_sync(true, Some(100), 101, debounce));
    }

    #[test]
    fn a_sweep_fence_spares_a_pull_request_synced_since_the_listing_started() {
        let resynced = PrState {
            last_synced_at: Some(1_000),
            ..PrState::default()
        };
        let sweep_began_at = |synced_before| ClosedRequest {
            synced_before: Some(synced_before),
        };

        assert!(
            !closed_retires(Some(&resynced), sweep_began_at(900)),
            "a webhook re-synced it after the sweep listed: it is open again"
        );
        assert!(
            !closed_retires(Some(&resynced), sweep_began_at(1_000)),
            "the boundary mirrors the projection's `synced_at < reconcile_start` guard"
        );
        assert!(closed_retires(Some(&resynced), sweep_began_at(1_001)));
    }

    #[test]
    fn a_webhook_closed_retires_regardless_of_sync_time() {
        let fresh = PrState {
            last_synced_at: Some(u64::MAX),
            ..PrState::default()
        };

        assert!(closed_retires(Some(&fresh), ClosedRequest::default()));
        assert!(closed_retires(None, ClosedRequest::default()));
    }

    #[test]
    fn an_unsynced_object_never_outranks_a_sweep_fence() {
        let never_synced = PrState {
            sync_pending: true,
            ..PrState::default()
        };
        let fence = ClosedRequest {
            synced_before: Some(900),
        };

        assert!(closed_retires(Some(&never_synced), fence));
        assert!(closed_retires(None, fence));
    }

    #[test]
    fn closed_accepts_an_empty_body_as_the_unconditional_form() {
        let mut legacy = bytes::Bytes::new();
        assert_eq!(
            <ClosedRequest as restate_sdk::serde::Deserialize>::deserialize(&mut legacy).unwrap(),
            ClosedRequest::default()
        );
        let mut fenced = bytes::Bytes::from_static(br#"{"synced_before":900}"#);
        assert_eq!(
            <ClosedRequest as restate_sdk::serde::Deserialize>::deserialize(&mut fenced).unwrap(),
            ClosedRequest {
                synced_before: Some(900)
            }
        );
    }
}
