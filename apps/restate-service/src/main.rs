use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    time::Duration,
};

use bytes::Bytes;
use dependaboard_core::{
    ActionLog, ActionOutcome, BatchProgress, BulkActionKind, BulkRequest, Classification,
    CommandRequest, DASHBOARD_SYNC_ACTION, DependabotCommand, GithubErrorResponse,
    MAX_BATCH_TARGETS, MergeRequest, Operation, PrKey, PrState, RejectReason, RepoRecord,
    SyncRequest, SyncShaRequest, TargetProgressState, UpdateBranchRequest, WebhookEvent,
    classify_github_error, unix_seconds, valid_batch_id,
};
use dependaboard_github::{GithubApi, GithubClient, GithubConfig, GithubError};
use dependaboard_store::{LibSqlPrStore, PrStore, StoreConfig, StoreError, StoreErrorClass};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

const PR_STATE: &str = "pr_state";
const BATCH_PROGRESS: &str = "progress";
const SCHEDULER_STARTED: &str = "scheduler_started";
const SCHEDULER_GENERATION: &str = "scheduler_generation";
const SCHEDULER_TICK_PENDING: &str = "scheduler_tick_pending";
const DEFAULT_DEBOUNCE_SECONDS: u64 = 20;
const DEFAULT_RECONCILE_SECONDS: u64 = 60 * 60;
const MAX_CONCURRENT: usize = 3;

#[derive(Clone)]
struct PullRequest {
    github: GithubClient,
    store: LibSqlPrStore,
    debounce: Duration,
}

#[restate_sdk::object]
impl PullRequest {
    #[handler(ingress_private)]
    async fn sync(&self, ctx: ObjectContext<'_>, request: Json<SyncRequest>) -> HandlerResult<()> {
        let request = request.into_inner();
        let now = ctx
            .run(|| async { Ok(unix_seconds()) })
            .name("sync-clock")
            .await?;
        let mut state = ctx
            .get::<Json<PrState>>(PR_STATE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        if should_debounce_sync(
            request.bypass_debounce,
            state.last_synced_at,
            now,
            self.debounce,
        ) {
            if !state.sync_pending {
                state.sync_pending = true;
                ctx.set(PR_STATE, Json::from(state));
                ctx.object_client::<PullRequestClient>(ctx.key())
                    .sync(Json::from(request))
                    .send_after(self.debounce);
            }
            return Ok(());
        }
        state.sync_pending = false;
        ctx.set(PR_STATE, Json::from(state.clone()));

        let github = self.github.clone();
        let sync_request = request.clone();
        let known_resource = state.snapshot.is_some();
        let fetched = ctx
            .run(move || async move {
                external(
                    github.fetch_snapshot(&sync_request).await,
                    Operation::Read,
                    known_resource,
                )
                .await
            })
            .retry_policy(github_retry_policy())
            .name("fetch-canonical-pr-snapshot")
            .await?
            .into_inner();
        let snapshot = match fetched {
            External::Ok(snapshot) => snapshot,
            External::Http {
                response,
                known_resource,
            } => match classify_github_error(&response, Operation::Read, known_resource) {
                Classification::Retryable { .. } => {
                    return Err(RetryableServiceError::Github(response.message).into());
                }
                Classification::Rejected(RejectReason::NotFound) => None,
                Classification::Rejected(reason) => {
                    return Err(TerminalError::new(reason.to_string()).into());
                }
                Classification::Fatal => {
                    return Err(TerminalError::new(format!(
                        "GitHub read failed with HTTP {}: {}",
                        response.status, response.message
                    ))
                    .into());
                }
            },
            External::StaleSha { .. } => {
                return Err(TerminalError::new("unexpected stale SHA while reading a PR").into());
            }
        };

        if let Some(snapshot) = snapshot {
            let store = self.store.clone();
            let projected = snapshot.clone();
            ctx.run(move || async move {
                store.upsert_pr(&projected).await.map_err(store_failure)?;
                Ok(())
            })
            .name("upsert-pr-projection")
            .await?;
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
            ctx.set(PR_STATE, Json::from(state));
        } else {
            let store = self.store.clone();
            let key = request_key(&request);
            ctx.run(move || async move {
                store.delete_pr(&key).await.map_err(store_failure)?;
                Ok(())
            })
            .name("delete-ineligible-pr-projection")
            .await?;
            ctx.clear_all();
        }
        Ok(())
    }

    #[handler(ingress_private)]
    async fn closed(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        let key = ctx
            .key()
            .parse::<PrKey>()
            .map_err(|error| TerminalError::new(error.to_string()))?;
        let store = self.store.clone();
        ctx.run(move || async move {
            store.delete_pr(&key).await.map_err(store_failure)?;
            Ok(())
        })
        .name("delete-closed-pr-projection")
        .await?;
        ctx.clear_all();
        Ok(())
    }

    #[handler(ingress_private)]
    async fn merge(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<MergeRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        let request = request.into_inner();
        let mut state = ctx
            .get::<Json<PrState>>(PR_STATE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let Some(snapshot) = state.snapshot.as_ref() else {
            return Ok(Json::from(rejected(RejectReason::NotFound)));
        };
        if !target_matches_snapshot(&request.target, snapshot) {
            return Ok(Json::from(rejected(RejectReason::NotFound)));
        }
        if snapshot.head_sha != request.target.expected_sha {
            return Ok(Json::from(rejected(RejectReason::StaleSha {
                expected: request.target.expected_sha,
                actual: snapshot.head_sha.clone(),
            })));
        }

        let github = self.github.clone();
        let merge_request = request.clone();
        let result = ctx
            .run(move || async move {
                external(github.merge(&merge_request).await, Operation::Merge, false).await
            })
            .retry_policy(github_retry_policy())
            .name("merge-pull-request")
            .await?
            .into_inner();
        let outcome = action_result(result, Operation::Merge)?;
        if matches!(outcome, ActionOutcome::Succeeded { .. }) {
            let store = self.store.clone();
            let key = request.target.key().parse::<PrKey>().map_err(|error| {
                TerminalError::new(format!("invalid merge target key: {error}"))
            })?;
            ctx.run(move || async move {
                store.delete_pr(&key).await.map_err(store_failure)?;
                Ok(())
            })
            .name("delete-merged-pr-projection")
            .await?;
            state.snapshot = None;
        }
        let log_at = ctx
            .run(|| async { Ok(unix_seconds()) })
            .name("merge-log-clock")
            .await?;
        state.push_history(ActionLog {
            at: log_at,
            action: "merge".to_owned(),
            detail: outcome_detail(&outcome),
        });
        ctx.set(PR_STATE, Json::from(state));
        Ok(Json::from(outcome))
    }

    #[handler(ingress_private)]
    async fn command(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<CommandRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        let request = request.into_inner();
        let mut state = ctx
            .get::<Json<PrState>>(PR_STATE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let Some(snapshot) = state.snapshot.as_ref() else {
            return Ok(Json::from(rejected(RejectReason::NotFound)));
        };
        if !target_matches_snapshot(&request.target, snapshot) {
            return Ok(Json::from(rejected(RejectReason::NotFound)));
        }
        if snapshot.head_sha != request.target.expected_sha {
            return Ok(Json::from(rejected(RejectReason::StaleSha {
                expected: request.target.expected_sha,
                actual: snapshot.head_sha.clone(),
            })));
        }

        let github = self.github.clone();
        let command_request = request.clone();
        let result = ctx
            .run(move || async move {
                external(
                    github.post_command(&command_request).await,
                    Operation::Comment,
                    false,
                )
                .await
            })
            .retry_policy(github_retry_policy())
            .name("post-dependabot-command")
            .await?
            .into_inner();
        let outcome = action_result(result, Operation::Comment)?;
        let log_at = ctx
            .run(|| async { Ok(unix_seconds()) })
            .name("command-log-clock")
            .await?;
        state.push_history(ActionLog {
            at: log_at,
            action: request.command.to_string(),
            detail: outcome_detail(&outcome),
        });
        ctx.set(PR_STATE, Json::from(state));
        Ok(Json::from(outcome))
    }

    #[handler(ingress_private)]
    async fn update_branch(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<UpdateBranchRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        let request = request.into_inner();
        let mut state = ctx
            .get::<Json<PrState>>(PR_STATE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let Some(snapshot) = state.snapshot.as_ref() else {
            return Ok(Json::from(rejected(RejectReason::NotFound)));
        };
        if !target_matches_snapshot(&request.target, snapshot) {
            return Ok(Json::from(rejected(RejectReason::NotFound)));
        }
        if snapshot.head_sha != request.target.expected_sha {
            return Ok(Json::from(rejected(RejectReason::StaleSha {
                expected: request.target.expected_sha,
                actual: snapshot.head_sha.clone(),
            })));
        }

        let github = self.github.clone();
        let update_request = request.clone();
        let result = ctx
            .run(move || async move {
                external(
                    github.update_branch(&update_request).await,
                    Operation::UpdateBranch,
                    false,
                )
                .await
            })
            .retry_policy(github_retry_policy())
            .name("update-pull-request-branch")
            .await?
            .into_inner();
        let outcome = action_result(result, Operation::UpdateBranch)?;
        let log_at = ctx
            .run(|| async { Ok(unix_seconds()) })
            .name("update-branch-log-clock")
            .await?;
        state.push_history(ActionLog {
            at: log_at,
            action: "update_branch".to_owned(),
            detail: outcome_detail(&outcome),
        });
        ctx.set(PR_STATE, Json::from(state));
        if matches!(outcome, ActionOutcome::Succeeded { .. }) {
            ctx.object_client::<PullRequestClient>(request.target.key())
                .sync(Json::from(SyncRequest {
                    repository_id: request.target.repository_id,
                    owner: request.target.owner,
                    repo: request.target.repo,
                    number: request.target.number,
                    observed_sha: None,
                    bypass_debounce: false,
                    completion_id: None,
                }))
                .send();
        }
        Ok(Json::from(outcome))
    }

    #[handler]
    async fn status(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<Option<PrState>>> {
        Ok(Json::from(
            ctx.get::<Json<PrState>>(PR_STATE)
                .await?
                .map(Json::into_inner),
        ))
    }
}

struct BulkAction;

#[restate_sdk::workflow(workflow_completion_retention = "7 days")]
impl BulkAction {
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<BulkRequest>,
    ) -> HandlerResult<Json<BatchProgress>> {
        let request = request.into_inner();
        validate_batch_request(ctx.key(), &request)?;
        let mut progress = BatchProgress::queued(ctx.key(), request.action, &request.targets);
        ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
        match request.action {
            BulkActionKind::Merge => {
                run_merge_batch(&ctx, &request, &mut progress).await?;
            }
            BulkActionKind::Rebase => {
                for target in &request.targets {
                    mark_running(&mut progress, &target.key());
                    ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
                    let outcome = match ctx
                        .object_client::<PullRequestClient>(target.key())
                        .command(Json::from(CommandRequest {
                            batch_id: ctx.key().to_owned(),
                            target: target.clone(),
                            user_id: request.user_id.clone(),
                            command: DependabotCommand::Rebase,
                        }))
                        .call()
                        .await
                    {
                        Ok(outcome) => outcome.into_inner(),
                        Err(error) => {
                            let detail = error.to_string();
                            mark_failed(&mut progress, &target.key(), detail.clone());
                            progress.fail(detail);
                            ctx.set(BATCH_PROGRESS, Json::from(progress));
                            return Err(error.into());
                        }
                    };
                    progress.record(&target.key(), outcome);
                    ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
                    if !progress.completed {
                        ctx.sleep(Duration::from_millis(350)).await?;
                    }
                }
            }
        }
        progress.completed = true;
        ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
        Ok(Json::from(progress))
    }

    #[handler]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> HandlerResult<Json<Option<BatchProgress>>> {
        Ok(Json::from(
            ctx.get::<Json<BatchProgress>>(BATCH_PROGRESS)
                .await?
                .map(Json::into_inner),
        ))
    }
}

async fn run_merge_batch(
    ctx: &WorkflowContext<'_>,
    request: &BulkRequest,
    progress: &mut BatchProgress,
) -> HandlerResult<()> {
    let mut by_repository: BTreeMap<u64, VecDeque<_>> = BTreeMap::new();
    for target in &request.targets {
        by_repository
            .entry(target.repository_id)
            .or_default()
            .push_back(target.clone());
    }
    while by_repository.values().any(|targets| !targets.is_empty()) {
        let round = by_repository
            .values_mut()
            .filter_map(VecDeque::pop_front)
            .take(MAX_CONCURRENT)
            .collect::<Vec<_>>();
        for target in &round {
            mark_running(progress, &target.key());
        }
        ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
        let mut calls = DurableFuturesUnordered::new();
        for target in &round {
            calls.push(
                ctx.object_client::<PullRequestClient>(target.key())
                    .merge(Json::from(MergeRequest {
                        batch_id: ctx.key().to_owned(),
                        target: target.clone(),
                    }))
                    .call(),
            );
        }
        let mut first_error = None;
        while let Some((index, outcome)) = calls.next().await? {
            let outcome = match outcome {
                Ok(outcome) => outcome.into_inner(),
                Err(error) => {
                    mark_failed(progress, &round[index].key(), error.to_string());
                    ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    continue;
                }
            };
            progress.record(&round[index].key(), outcome);
            ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
        }
        if let Some(error) = first_error {
            progress.fail(error.to_string());
            ctx.set(BATCH_PROGRESS, Json::from(progress.clone()));
            return Err(error.into());
        }
    }
    Ok(())
}

fn mark_running(progress: &mut BatchProgress, key: &str) {
    if let Some(target) = progress
        .targets
        .iter_mut()
        .find(|target| target.target.key() == key)
    {
        target.state = TargetProgressState::Running;
    }
}

fn mark_failed(progress: &mut BatchProgress, key: &str, detail: String) {
    if let Some(target) = progress
        .targets
        .iter_mut()
        .find(|target| target.target.key() == key)
    {
        target.state = TargetProgressState::Failed { detail };
    }
}

#[derive(Clone)]
struct InstallationSync {
    github: GithubClient,
    store: LibSqlPrStore,
    interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SchedulerTick(Option<u64>);

impl restate_sdk::serde::Serialize for SchedulerTick {
    type Error = serde_json::Error;

    fn serialize(&self) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(&self.0).map(Bytes::from)
    }
}

impl restate_sdk::serde::Deserialize for SchedulerTick {
    type Error = serde_json::Error;

    fn deserialize(bytes: &mut Bytes) -> Result<Self, Self::Error> {
        if bytes.is_empty() {
            Ok(Self(None))
        } else {
            serde_json::from_slice(bytes).map(Self)
        }
    }
}

impl restate_sdk::serde::PayloadMetadata for SchedulerTick {
    fn json_schema() -> Option<serde_json::Value> {
        Some(serde_json::json!({ "type": ["integer", "null"], "minimum": 0 }))
    }
}

#[restate_sdk::object(ingress_private)]
impl InstallationSync {
    #[handler]
    async fn start(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        let state = read_scheduler_state(&ctx).await?;
        let Some(next) = scheduler_start_transition(state)? else {
            return Ok(());
        };
        write_scheduler_state(&ctx, next);
        ctx.object_client::<InstallationSyncClient>(ctx.key())
            .tick(SchedulerTick(Some(next.generation)))
            .send();
        Ok(())
    }

    #[handler]
    async fn tick(&self, ctx: ObjectContext<'_>, generation: SchedulerTick) -> HandlerResult<()> {
        let state = read_scheduler_state(&ctx).await?;
        let mut restate = RestateTickEffects {
            ctx: &ctx,
            github: &self.github,
            store: &self.store,
            interval: self.interval,
        };
        run_scheduler_tick(&mut restate, state, generation).await
    }

    #[handler]
    async fn sync_now(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        perform_installation_sync(&ctx, self.github.clone(), self.store.clone()).await
    }

    #[handler]
    async fn pause(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        ctx.clear(SCHEDULER_STARTED);
        ctx.clear(SCHEDULER_TICK_PENDING);
        invalidate_scheduler_generation(&ctx).await?;
        Ok(())
    }

    #[handler]
    async fn purge(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        ctx.clear(SCHEDULER_STARTED);
        ctx.clear(SCHEDULER_TICK_PENDING);
        invalidate_scheduler_generation(&ctx).await?;
        let installation_id = ctx
            .key()
            .parse::<u64>()
            .map_err(|_| TerminalError::new("installation key must be an integer"))?;
        let store = self.store.clone();
        ctx.run(move || async move {
            store
                .purge_installation(installation_id)
                .await
                .map_err(store_failure)?;
            Ok(())
        })
        .name("purge-installation")
        .await?;
        Ok(())
    }
}

fn next_scheduler_generation(current: u64) -> HandlerResult<u64> {
    current
        .checked_add(1)
        .ok_or_else(|| TerminalError::new("scheduler generation overflow").into())
}

/// Scheduler state persisted on the `InstallationSync` object.
///
/// `tick_pending` promises that a tick for `generation` is queued or delayed inside
/// Restate; `start` relies on it to stay idempotent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SchedulerState {
    started: bool,
    tick_pending: bool,
    generation: u64,
}

impl SchedulerState {
    /// A live chain with exactly one tick for `generation` inside Restate.
    fn armed(generation: u64) -> Self {
        Self {
            started: true,
            tick_pending: true,
            generation,
        }
    }
}

async fn read_scheduler_state(ctx: &ObjectContext<'_>) -> HandlerResult<SchedulerState> {
    Ok(SchedulerState {
        started: ctx.get::<bool>(SCHEDULER_STARTED).await?.unwrap_or(false),
        tick_pending: ctx
            .get::<bool>(SCHEDULER_TICK_PENDING)
            .await?
            .unwrap_or(false),
        generation: ctx.get::<u64>(SCHEDULER_GENERATION).await?.unwrap_or(0),
    })
}

fn write_scheduler_state(ctx: &ObjectContext<'_>, state: SchedulerState) {
    ctx.set(SCHEDULER_GENERATION, state.generation);
    ctx.set(SCHEDULER_STARTED, state.started);
    ctx.set(SCHEDULER_TICK_PENDING, state.tick_pending);
}

/// What `tick` must do, decided before any side effect runs.
/// Decides whether `start` must (re)arm the chain, and with which generation.
///
/// `None` means a tick is already queued or delayed for the current generation, so
/// starting again would fork a second perpetual chain.
fn scheduler_start_transition(state: SchedulerState) -> HandlerResult<Option<SchedulerState>> {
    if state.started && state.tick_pending {
        return Ok(None);
    }
    let generation = if state.started && state.generation > 0 {
        state.generation
    } else {
        next_scheduler_generation(state.generation)?
    };
    Ok(Some(SchedulerState::armed(generation)))
}

/// Decides whether `tick` must re-arm the chain and sweep, and with which generation.
///
/// `None` drops the tick without touching state: the chain is paused, the generation is
/// stale (`pause`/`purge` bumped it), or a legacy generation-less tick arrived while a
/// current tick is already pending. `Some(state)` is what to persist before sweeping.
fn scheduler_tick_transition(
    state: SchedulerState,
    incoming: SchedulerTick,
) -> HandlerResult<Option<SchedulerState>> {
    if !state.started {
        return Ok(None);
    }
    let generation = match incoming.0 {
        Some(generation) if generation == state.generation => generation,
        Some(_) => return Ok(None),
        None if state.tick_pending => return Ok(None),
        None => next_scheduler_generation(state.generation)?,
    };
    Ok(Some(SchedulerState::armed(generation)))
}

/// Side effects a tick asks of Restate, abstracted so `run_scheduler_tick` can be
/// exercised against a recording fake without a runtime.
trait SchedulerTickEffects {
    fn installation_id(&self) -> &str;
    fn persist(&mut self, state: SchedulerState);
    fn schedule_tick(&mut self, generation: u64);
    fn sweep(&mut self) -> impl Future<Output = HandlerResult<()>> + Send;
}

struct RestateTickEffects<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    github: &'a GithubClient,
    store: &'a LibSqlPrStore,
    interval: Duration,
}

impl SchedulerTickEffects for RestateTickEffects<'_, '_> {
    fn installation_id(&self) -> &str {
        self.ctx.key()
    }

    fn persist(&mut self, state: SchedulerState) {
        write_scheduler_state(self.ctx, state);
    }

    fn schedule_tick(&mut self, generation: u64) {
        self.ctx
            .object_client::<InstallationSyncClient>(self.ctx.key())
            .tick(SchedulerTick(Some(generation)))
            .send_after(self.interval);
    }

    async fn sweep(&mut self) -> HandlerResult<()> {
        perform_installation_sync(self.ctx, self.github.clone(), self.store.clone()).await
    }
}

async fn run_scheduler_tick<E: SchedulerTickEffects>(
    restate: &mut E,
    state: SchedulerState,
    incoming: SchedulerTick,
) -> HandlerResult<()> {
    let Some(next) = scheduler_tick_transition(state, incoming)? else {
        return Ok(());
    };
    // Re-arm before sweeping. Restate never rolls back journaled state or sends, so the
    // chain survives a terminal or aborted sweep instead of dying silently.
    restate.persist(next);
    restate.schedule_tick(next.generation);
    if let Err(error) = restate.sweep().await {
        // `HandlerError` only renders through `AsRef<dyn Error>`; its message already says
        // whether Restate treated the failure as terminal or retryable.
        let cause: &dyn std::error::Error = error.as_ref();
        warn!(
            installation_id = restate.installation_id(),
            %cause,
            "installation reconcile failed; the next tick is already scheduled"
        );
        return Err(error);
    }
    Ok(())
}

async fn invalidate_scheduler_generation(ctx: &ObjectContext<'_>) -> HandlerResult<()> {
    let generation =
        next_scheduler_generation(ctx.get::<u64>(SCHEDULER_GENERATION).await?.unwrap_or(0))?;
    ctx.set(SCHEDULER_GENERATION, generation);
    Ok(())
}

async fn perform_installation_sync(
    ctx: &ObjectContext<'_>,
    github: GithubClient,
    store: LibSqlPrStore,
) -> HandlerResult<()> {
    let reconcile_start = ctx
        .run(|| async { Ok(unix_seconds()) })
        .name("installation-reconcile-clock")
        .await?;
    let list_client = github.clone();
    let repositories = ctx
        .run(move || async move {
            external(
                list_client.list_installation_repositories().await,
                Operation::Read,
                false,
            )
            .await
        })
        .retry_policy(github_retry_policy())
        .name("list-installation-repositories")
        .await?
        .into_inner();
    let repositories = external_read(repositories)?;
    let stored = repositories.clone();
    let installation_id = github.installation_id();
    ctx.run(move || async move {
        store
            .replace_installation_repos(installation_id, &stored, reconcile_start)
            .await
            .map_err(store_failure)?;
        Ok(())
    })
    .name("replace-installation-repositories")
    .await?;
    for repository in repositories {
        ctx.object_client::<RepoSyncClient>(repository.repository_id.to_string())
            .reconcile(Json::from(repository))
            .send();
    }
    Ok(())
}

/// Side effects a repository reconcile asks of Restate, GitHub and the store, abstracted
/// so `run_repo_reconcile` can be exercised against a recording fake without a runtime.
trait RepoReconcileEffects {
    fn repository_id(&self) -> u64;
    fn list_pull_requests(
        &mut self,
    ) -> impl Future<Output = HandlerResult<Vec<SyncRequest>>> + Send;
    /// A Restate-to-Restate call only fails once the callee has failed terminally;
    /// retryable failures are retried inside `PullRequest::sync` and never surface here.
    fn sync_pull_request(
        &mut self,
        request: &SyncRequest,
    ) -> impl Future<Output = Result<(), TerminalError>> + Send;
    fn retain_pull_requests(
        &mut self,
        live: &[u64],
    ) -> impl Future<Output = HandlerResult<()>> + Send;
}

struct RestateReconcileEffects<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    github: &'a GithubClient,
    store: &'a LibSqlPrStore,
    repository: RepoRecord,
    reconcile_start: u64,
}

impl RepoReconcileEffects for RestateReconcileEffects<'_, '_> {
    fn repository_id(&self) -> u64 {
        self.repository.repository_id
    }

    async fn list_pull_requests(&mut self) -> HandlerResult<Vec<SyncRequest>> {
        let github = self.github.clone();
        let owner = self.repository.owner.clone();
        let repo = self.repository.repo.clone();
        let repository_id = self.repository_id();
        let pulls = self
            .ctx
            .run(move || async move {
                external(
                    github
                        .list_dependabot_prs(&owner, &repo, repository_id)
                        .await,
                    Operation::Read,
                    false,
                )
                .await
            })
            .retry_policy(github_retry_policy())
            .name("list-open-dependabot-prs")
            .await?
            .into_inner();
        external_read(pulls)
    }

    async fn sync_pull_request(&mut self, request: &SyncRequest) -> Result<(), TerminalError> {
        self.ctx
            .object_client::<PullRequestClient>(request_key(request).to_string())
            .sync(Json::from(request.clone()))
            .call()
            .await
    }

    async fn retain_pull_requests(&mut self, live: &[u64]) -> HandlerResult<()> {
        let store = self.store.clone();
        let repository_id = self.repository_id();
        let reconcile_start = self.reconcile_start;
        let live = live.to_vec();
        self.ctx
            .run(move || async move {
                store
                    .retain_prs(repository_id, &live, reconcile_start)
                    .await
                    .map_err(store_failure)?;
                Ok(())
            })
            .name("retain-live-pull-requests")
            .await?;
        Ok(())
    }
}

/// Sweeps every listed pull request, then prunes the projection down to the listing.
///
/// A pull request that fails terminally is logged and remembered rather than propagated,
/// so one unsyncable pull request can neither starve the rest of the repository nor skip
/// stale-row cleanup. Retention still requires a complete listing: if listing fails, the
/// live set is unknown and nothing is deleted.
async fn run_repo_reconcile<E: RepoReconcileEffects>(restate: &mut E) -> HandlerResult<()> {
    let pulls = restate.list_pull_requests().await?;
    let mut failed = Vec::new();
    for request in &pulls {
        if let Err(error) = restate.sync_pull_request(request).await {
            let key = request_key(request);
            warn!(
                repository_id = restate.repository_id(),
                pull_request = %key,
                cause = %error,
                "pull request sync failed; continuing the repository reconcile"
            );
            failed.push(key);
        }
    }
    let live = pulls
        .iter()
        .map(|request| request.number)
        .collect::<Vec<_>>();
    restate.retain_pull_requests(&live).await?;
    if failed.is_empty() {
        return Ok(());
    }
    let keys = failed
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Err(TerminalError::new(format!(
        "{} of {} pull request syncs failed while reconciling repository {}: {keys}",
        failed.len(),
        pulls.len(),
        restate.repository_id(),
    ))
    .into())
}

#[derive(Clone)]
struct RepoSync {
    github: GithubClient,
    store: LibSqlPrStore,
}

#[restate_sdk::object(ingress_private)]
impl RepoSync {
    #[handler]
    async fn reconcile(
        &self,
        ctx: ObjectContext<'_>,
        repository: Json<RepoRecord>,
    ) -> HandlerResult<()> {
        let repository = repository.into_inner();
        let reconcile_start = ctx
            .run(|| async { Ok(unix_seconds()) })
            .name("repo-reconcile-clock")
            .await?;
        let mut restate = RestateReconcileEffects {
            ctx: &ctx,
            github: &self.github,
            store: &self.store,
            repository,
            reconcile_start,
        };
        run_repo_reconcile(&mut restate).await
    }

    #[handler]
    async fn sync_sha(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<SyncShaRequest>,
    ) -> HandlerResult<()> {
        let request = request.into_inner();
        let store = self.store.clone();
        let repository_id = request.repository_id;
        let sha = request.sha.clone();
        let matches = ctx
            .run(move || async move {
                Ok(Json::from(
                    store
                        .prs_for_sha(repository_id, &sha)
                        .await
                        .map_err(store_failure)?,
                ))
            })
            .name("resolve-prs-for-sha")
            .await?;
        let mut numbers = request
            .pull_requests
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        numbers.extend(matches.into_inner().into_iter().map(|pull| pull.number));
        for number in numbers {
            let sync = SyncRequest {
                repository_id: request.repository_id,
                owner: request.owner.clone(),
                repo: request.repo.clone(),
                number,
                observed_sha: Some(request.sha.clone()),
                bypass_debounce: false,
                completion_id: None,
            };
            ctx.object_client::<PullRequestClient>(request_key(&sync).to_string())
                .sync(Json::from(sync))
                .send();
        }
        Ok(())
    }
}

struct WebhookIngress {
    installation_id: u64,
}

struct SchedulerIngress {
    installation_id: u64,
}

#[restate_sdk::service]
impl SchedulerIngress {
    #[handler]
    async fn start(&self, ctx: Context<'_>) -> HandlerResult<()> {
        ctx.object_client::<InstallationSyncClient>(self.installation_id.to_string())
            .start()
            .send();
        Ok(())
    }
}

#[restate_sdk::service]
impl WebhookIngress {
    #[handler]
    async fn dispatch(&self, ctx: Context<'_>, event: Json<WebhookEvent>) -> HandlerResult<()> {
        let event = event.into_inner();
        if event
            .installation_id
            .is_some_and(|installation_id| installation_id != self.installation_id)
        {
            return Err(
                TerminalError::new("webhook installation does not match configuration").into(),
            );
        }
        match event.event.as_str() {
            "pull_request" => dispatch_pull_request(&ctx, &event)?,
            "check_suite" if event.action.as_deref() == Some("completed") => {
                dispatch_sha(&ctx, &event)?
            }
            "check_run" if matches!(event.action.as_deref(), Some("created" | "completed")) => {
                dispatch_sha(&ctx, &event)?
            }
            "status" => dispatch_sha(&ctx, &event)?,
            "installation" => dispatch_installation(&ctx, &event)?,
            "installation_repositories" => {
                if let Some(installation_id) = event.installation_id {
                    ctx.object_client::<InstallationSyncClient>(installation_id.to_string())
                        .sync_now()
                        .send();
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn dispatch_pull_request(ctx: &Context<'_>, event: &WebhookEvent) -> HandlerResult<()> {
    let (Some(repository_id), Some(owner), Some(repo), Some(number)) = (
        event.repository_id,
        event.owner.as_ref(),
        event.repo.as_ref(),
        event.number,
    ) else {
        return Err(TerminalError::new("pull_request webhook is missing routing fields").into());
    };
    let key = PrKey::new(repository_id, number).to_string();
    match event.action.as_deref() {
        Some("closed") => {
            ctx.object_client::<PullRequestClient>(key).closed().send();
        }
        action if pull_request_action_requests_sync(action) => {
            ctx.object_client::<PullRequestClient>(key)
                .sync(Json::from(SyncRequest {
                    repository_id,
                    owner: owner.clone(),
                    repo: repo.clone(),
                    number,
                    observed_sha: event.sha.clone(),
                    bypass_debounce: event.action.as_deref() == Some(DASHBOARD_SYNC_ACTION),
                    completion_id: event.sync_completion_id.clone(),
                }))
                .send();
        }
        _ => {}
    }
    Ok(())
}

fn pull_request_action_requests_sync(action: Option<&str>) -> bool {
    matches!(
        action,
        Some(
            "opened"
                | "reopened"
                | "synchronize"
                | "edited"
                | "labeled"
                | "unlabeled"
                | DASHBOARD_SYNC_ACTION
        )
    )
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

fn dispatch_sha(ctx: &Context<'_>, event: &WebhookEvent) -> HandlerResult<()> {
    let (Some(repository_id), Some(owner), Some(repo), Some(sha)) = (
        event.repository_id,
        event.owner.as_ref(),
        event.repo.as_ref(),
        event.sha.as_ref(),
    ) else {
        return Err(TerminalError::new("commit webhook is missing routing fields").into());
    };
    ctx.object_client::<RepoSyncClient>(repository_id.to_string())
        .sync_sha(Json::from(SyncShaRequest {
            repository_id,
            owner: owner.clone(),
            repo: repo.clone(),
            sha: sha.clone(),
            pull_requests: event.pull_requests.clone(),
        }))
        .send();
    Ok(())
}

fn dispatch_installation(ctx: &Context<'_>, event: &WebhookEvent) -> HandlerResult<()> {
    let Some(installation_id) = event.installation_id else {
        return Err(TerminalError::new("installation webhook is missing installation id").into());
    };
    let client = ctx.object_client::<InstallationSyncClient>(installation_id.to_string());
    match installation_lifecycle_action(event.action.as_deref()) {
        InstallationLifecycleAction::Start => {
            client.start().send();
        }
        InstallationLifecycleAction::SyncNow => {
            client.sync_now().send();
        }
        InstallationLifecycleAction::Pause => {
            client.pause().send();
        }
        InstallationLifecycleAction::Purge => {
            client.purge().send();
        }
        InstallationLifecycleAction::Ignore => {}
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallationLifecycleAction {
    Start,
    SyncNow,
    Pause,
    Purge,
    Ignore,
}

fn installation_lifecycle_action(action: Option<&str>) -> InstallationLifecycleAction {
    match action {
        Some("created") => InstallationLifecycleAction::SyncNow,
        Some("unsuspend") => InstallationLifecycleAction::Start,
        Some("suspend") => InstallationLifecycleAction::Pause,
        Some("deleted") => InstallationLifecycleAction::Purge,
        _ => InstallationLifecycleAction::Ignore,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum External<T> {
    Ok(T),
    Http {
        response: GithubErrorResponse,
        known_resource: bool,
    },
    StaleSha {
        expected: String,
        actual: String,
    },
}

async fn external<T>(
    result: Result<T, GithubError>,
    operation: Operation,
    known_resource: bool,
) -> HandlerResult<Json<External<T>>>
where
    T: Serialize + for<'de> Deserialize<'de> + 'static,
{
    match result {
        Ok(value) => Ok(Json::from(External::Ok(value))),
        Err(GithubError::Http(response)) => {
            if let Classification::Retryable { after_seconds } =
                classify_github_error(&response, operation, known_resource)
            {
                honor_retry_after(after_seconds).await;
                Err(RetryableServiceError::Github(response.message).into())
            } else {
                Ok(Json::from(External::Http {
                    response,
                    known_resource,
                }))
            }
        }
        Err(GithubError::HttpKnown(response)) => {
            if let Classification::Retryable { after_seconds } =
                classify_github_error(&response, operation, true)
            {
                honor_retry_after(after_seconds).await;
                Err(RetryableServiceError::Github(response.message).into())
            } else {
                Ok(Json::from(External::Http {
                    response,
                    known_resource: true,
                }))
            }
        }
        Err(GithubError::StaleSha { expected, actual }) => {
            Ok(Json::from(External::StaleSha { expected, actual }))
        }
        Err(GithubError::Transport(message)) => Err(RetryableServiceError::Github(message).into()),
        Err(GithubError::Protocol(message) | GithubError::Config(message)) => {
            Err(TerminalError::new(message).into())
        }
    }
}

async fn honor_retry_after(after_seconds: Option<u64>) {
    if let Some(after_seconds) = after_seconds.filter(|seconds| *seconds > 0) {
        tokio::time::sleep(Duration::from_secs(after_seconds)).await;
    }
}

fn github_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5 * 60))
}

fn store_failure(error: StoreError) -> HandlerError {
    match error.class() {
        StoreErrorClass::Retryable => RetryableServiceError::Store(error.to_string()).into(),
        StoreErrorClass::Terminal => TerminalError::new(error.to_string()).into(),
    }
}

fn external_read<T>(result: External<T>) -> HandlerResult<T> {
    match result {
        External::Ok(value) => Ok(value),
        External::Http {
            response,
            known_resource,
        } => match classify_github_error(&response, Operation::Read, known_resource) {
            Classification::Retryable { .. } => {
                Err(RetryableServiceError::Github(response.message).into())
            }
            Classification::Rejected(reason) => Err(TerminalError::new(reason.to_string()).into()),
            Classification::Fatal => Err(TerminalError::new(format!(
                "GitHub read failed with HTTP {}: {}",
                response.status, response.message
            ))
            .into()),
        },
        External::StaleSha { .. } => {
            Err(TerminalError::new("unexpected stale SHA while reading GitHub").into())
        }
    }
}

fn action_result(result: External<String>, operation: Operation) -> HandlerResult<ActionOutcome> {
    match result {
        External::Ok(detail) => Ok(ActionOutcome::Succeeded { detail }),
        External::StaleSha { expected, actual } => {
            Ok(rejected(RejectReason::StaleSha { expected, actual }))
        }
        External::Http {
            response,
            known_resource,
        } => match classify_github_error(&response, operation, known_resource) {
            Classification::Retryable { .. } => {
                Err(RetryableServiceError::Github(response.message).into())
            }
            Classification::Rejected(reason) => Ok(rejected(reason)),
            Classification::Fatal => Err(TerminalError::new(format!(
                "GitHub mutation failed with HTTP {}: {}",
                response.status, response.message
            ))
            .into()),
        },
    }
}

fn rejected(reason: RejectReason) -> ActionOutcome {
    ActionOutcome::Rejected { reason }
}

fn outcome_detail(outcome: &ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Succeeded { detail } => detail.clone(),
        ActionOutcome::Rejected { reason } => reason.to_string(),
    }
}

fn request_key(request: &SyncRequest) -> PrKey {
    PrKey::new(request.repository_id, request.number)
}

fn target_matches_snapshot(
    target: &dependaboard_core::PrTarget,
    snapshot: &dependaboard_core::PrRecord,
) -> bool {
    target.repository_id == snapshot.repository_id
        && target.owner == snapshot.owner
        && target.repo == snapshot.repo
        && target.number == snapshot.number
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

fn short_sha(value: &str) -> &str {
    value.get(..7).unwrap_or(value)
}

#[derive(Debug, Error)]
enum RetryableServiceError {
    #[error("retryable GitHub failure: {0}")]
    Github(String),
    #[error("retryable projection-store failure: {0}")]
    Store(String),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let store = LibSqlPrStore::connect(&StoreConfig::from_env()).await?;
    let github = GithubClient::new(GithubConfig::from_env()?)?;
    let debounce = Duration::from_secs(env_u64("SYNC_DEBOUNCE_SECONDS", DEFAULT_DEBOUNCE_SECONDS));
    let interval = Duration::from_secs(env_u64(
        "RECONCILE_INTERVAL_SECONDS",
        DEFAULT_RECONCILE_SECONDS,
    ));
    let installation_id = github.installation_id();

    let pull_request = PullRequest {
        github: github.clone(),
        store: store.clone(),
        debounce,
    };
    let installation_sync = InstallationSync {
        github: github.clone(),
        store: store.clone(),
        interval,
    };
    let repo_sync = RepoSync { github, store };
    let endpoint = Endpoint::builder()
        .bind(pull_request)
        .bind(BulkAction)
        .bind(WebhookIngress { installation_id })
        .bind(SchedulerIngress { installation_id })
        .bind(installation_sync)
        .bind(repo_sync)
        .build();

    tokio::spawn(start_scheduler(installation_id));
    let address = env::var("RESTATE_SERVICE_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:9080".to_owned())
        .parse()?;
    info!(%address, "starting Restate service endpoint");
    HttpServer::new(endpoint).listen_and_serve(address).await;
    Ok(())
}

async fn start_scheduler(installation_id: u64) {
    let ingress = env::var("RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
        .trim_end_matches('/')
        .to_owned();
    let api_key = env::var("RESTATE_AUTH_TOKEN")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            env::var("RESTATE_API_KEY")
                .ok()
                .filter(|value| !value.is_empty())
        });
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "could not build Restate scheduler client");
            return;
        }
    };
    let url = format!("{ingress}/restate/send/SchedulerIngress/start");
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let request = scheduler_start_request(&client, &url, api_key.as_deref());
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!(
                    installation_id,
                    "installation scheduler accepted by Restate"
                );
                return;
            }
            Ok(response) => {
                warn!(status = %response.status(), "Restate has not accepted scheduler startup yet")
            }
            Err(error) => warn!(%error, "Restate ingress is not ready for scheduler startup"),
        }
    }
}

fn scheduler_start_request(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut request = client.post(url);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    request
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use dependaboard_core::{CheckStatus, Mergeable, PrRecord, PrTarget, UpdateType, UserId};
    use restate_sdk::service::Discoverable;

    use super::*;

    fn target() -> PrTarget {
        PrTarget {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            expected_sha: "abc123".to_owned(),
            title: "Bump serde".to_owned(),
            html_url: "https://github.com/acme/api/pull/9".to_owned(),
        }
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
    fn scheduler_start_request_has_no_input_payload() {
        let request = scheduler_start_request(
            &reqwest::Client::new(),
            "http://127.0.0.1:8080/restate/send/SchedulerIngress/start",
            None,
        )
        .build()
        .expect("scheduler request should build");

        assert!(request.body().is_none());
        assert!(
            !request
                .headers()
                .contains_key(reqwest::header::CONTENT_TYPE)
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
    fn target_routing_must_match_the_canonical_snapshot() {
        let snapshot = PrRecord {
            id: "7#9".to_owned(),
            repository_id: 7,
            installation_id: 1,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            title: "Bump serde".to_owned(),
            html_url: "https://github.com/acme/api/pull/9".to_owned(),
            dependency: Some("serde".to_owned()),
            from_version: None,
            to_version: None,
            dependencies: Vec::new(),
            update_type: UpdateType::Unknown,
            head_sha: "abc123".to_owned(),
            check_status: CheckStatus::None,
            mergeable: Mergeable::Unknown,
            labels: Vec::new(),
            created_at: 0,
            updated_at: 0,
            synced_at: 0,
        };
        assert!(target_matches_snapshot(&target(), &snapshot));
        let mut wrong = target();
        wrong.repo = "other".to_owned();
        assert!(!target_matches_snapshot(&wrong, &snapshot));
    }

    #[tokio::test]
    async fn retryable_http_responses_fail_inside_the_journaled_run() {
        let response = GithubErrorResponse {
            status: 503,
            message: "unavailable".to_owned(),
            ..Default::default()
        };
        assert!(
            external::<()>(Err(GithubError::Http(response)), Operation::Read, false)
                .await
                .is_err()
        );
    }

    #[test]
    fn installation_webhooks_map_to_distinct_lifecycle_actions() {
        assert_eq!(
            installation_lifecycle_action(Some("created")),
            InstallationLifecycleAction::SyncNow
        );
        assert_eq!(
            installation_lifecycle_action(Some("unsuspend")),
            InstallationLifecycleAction::Start
        );
        assert_eq!(
            installation_lifecycle_action(Some("suspend")),
            InstallationLifecycleAction::Pause
        );
        assert_eq!(
            installation_lifecycle_action(Some("deleted")),
            InstallationLifecycleAction::Purge
        );
    }

    #[test]
    fn dashboard_action_requests_a_pull_request_sync() {
        assert!(pull_request_action_requests_sync(Some(
            DASHBOARD_SYNC_ACTION
        )));
        assert!(!pull_request_action_requests_sync(Some("closed")));
        assert!(!pull_request_action_requests_sync(None));
    }

    #[test]
    fn dashboard_sync_bypasses_the_event_debounce() {
        let debounce = Duration::from_secs(20);
        assert!(should_debounce_sync(false, Some(100), 101, debounce));
        assert!(!should_debounce_sync(true, Some(100), 101, debounce));
    }

    #[test]
    fn scheduler_generations_are_monotonic_and_cannot_wrap() {
        assert_eq!(next_scheduler_generation(41).unwrap(), 42);
        assert!(next_scheduler_generation(u64::MAX).is_err());
    }

    #[test]
    fn current_tick_rearms_the_chain_without_clearing_the_pending_flag() {
        assert_eq!(
            scheduler_tick_transition(SchedulerState::armed(42), SchedulerTick(Some(42))).unwrap(),
            Some(SchedulerState {
                started: true,
                tick_pending: true,
                generation: 42,
            })
        );
    }

    #[test]
    fn stale_or_paused_ticks_are_dropped_without_touching_state() {
        // `pause`/`purge` bump the generation, so a delayed tick from before that carries
        // an older generation and must die even if `start` re-armed the chain since.
        assert_eq!(
            scheduler_tick_transition(SchedulerState::armed(43), SchedulerTick(Some(42))).unwrap(),
            None
        );
        let paused = SchedulerState {
            started: false,
            tick_pending: false,
            generation: 43,
        };
        assert_eq!(
            scheduler_tick_transition(paused, SchedulerTick(Some(43))).unwrap(),
            None
        );
    }

    #[test]
    fn legacy_ticks_yield_to_a_pending_tick_or_adopt_a_fresh_generation() {
        assert_eq!(
            scheduler_tick_transition(SchedulerState::armed(42), SchedulerTick(None)).unwrap(),
            None
        );
        let orphaned = SchedulerState {
            started: true,
            tick_pending: false,
            generation: 42,
        };
        assert_eq!(
            scheduler_tick_transition(orphaned, SchedulerTick(None)).unwrap(),
            Some(SchedulerState::armed(43))
        );
    }

    /// Stands in for the Restate object context and records what a tick asked of it.
    #[derive(Default)]
    struct RecordedRestate {
        persisted: Option<SchedulerState>,
        scheduled: Vec<u64>,
        swept: bool,
        sweep_failure: Option<HandlerError>,
    }

    impl SchedulerTickEffects for RecordedRestate {
        fn installation_id(&self) -> &str {
            "1"
        }

        fn persist(&mut self, state: SchedulerState) {
            self.persisted = Some(state);
        }

        fn schedule_tick(&mut self, generation: u64) {
            self.scheduled.push(generation);
        }

        async fn sweep(&mut self) -> HandlerResult<()> {
            self.swept = true;
            match self.sweep_failure.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    #[tokio::test]
    async fn a_terminal_sweep_failure_leaves_the_next_tick_scheduled() {
        let mut restate = RecordedRestate {
            sweep_failure: Some(
                TerminalError::new("GitHub read failed with HTTP 401: Bad credentials").into(),
            ),
            ..Default::default()
        };

        let outcome = run_scheduler_tick(
            &mut restate,
            SchedulerState::armed(42),
            SchedulerTick(Some(42)),
        )
        .await;

        assert!(
            outcome.is_err(),
            "the failed sweep stays visible to Restate"
        );
        assert_eq!(restate.scheduled, vec![42]);
        assert_eq!(restate.persisted, Some(SchedulerState::armed(42)));
    }

    #[tokio::test]
    async fn a_successful_tick_sweeps_and_schedules_exactly_one_successor() {
        let mut restate = RecordedRestate::default();

        run_scheduler_tick(
            &mut restate,
            SchedulerState::armed(42),
            SchedulerTick(Some(42)),
        )
        .await
        .unwrap();

        assert!(restate.swept);
        assert_eq!(restate.scheduled, vec![42]);
        assert_eq!(restate.persisted, Some(SchedulerState::armed(42)));
    }

    #[tokio::test]
    async fn a_stale_tick_neither_sweeps_nor_schedules() {
        let mut restate = RecordedRestate::default();

        run_scheduler_tick(
            &mut restate,
            SchedulerState::armed(43),
            SchedulerTick(Some(42)),
        )
        .await
        .unwrap();

        assert!(!restate.swept);
        assert!(restate.scheduled.is_empty());
        assert_eq!(restate.persisted, None);
    }

    #[tokio::test]
    async fn start_is_a_no_op_on_the_state_a_failed_sweep_leaves_behind() {
        let mut restate = RecordedRestate {
            sweep_failure: Some(TerminalError::new("projection store is read-only").into()),
            ..Default::default()
        };
        let _ = run_scheduler_tick(
            &mut restate,
            SchedulerState::armed(42),
            SchedulerTick(Some(42)),
        )
        .await;

        let after_failure = restate.persisted.expect("tick persisted its state");
        assert_eq!(scheduler_start_transition(after_failure).unwrap(), None);
    }

    #[test]
    fn start_revives_a_paused_or_orphaned_chain_with_a_valid_generation() {
        let paused = SchedulerState {
            started: false,
            tick_pending: false,
            generation: 42,
        };
        assert_eq!(
            scheduler_start_transition(paused).unwrap(),
            Some(SchedulerState::armed(43))
        );
        let orphaned = SchedulerState {
            started: true,
            tick_pending: false,
            generation: 42,
        };
        assert_eq!(
            scheduler_start_transition(orphaned).unwrap(),
            Some(SchedulerState::armed(42))
        );
        assert_eq!(
            scheduler_start_transition(SchedulerState::default()).unwrap(),
            Some(SchedulerState::armed(1))
        );
    }

    fn dependabot_pull(number: u64) -> SyncRequest {
        SyncRequest {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number,
            observed_sha: None,
            bypass_debounce: false,
            completion_id: None,
        }
    }

    /// Stands in for Restate, GitHub and the store during a repository reconcile and
    /// records what the sweep asked of them.
    #[derive(Default)]
    struct RecordedRepoSync {
        pulls: Vec<SyncRequest>,
        listing_failure: Option<HandlerError>,
        sync_failures: BTreeMap<u64, TerminalError>,
        synced: Vec<u64>,
        retained: Option<Vec<u64>>,
    }

    impl RepoReconcileEffects for RecordedRepoSync {
        fn repository_id(&self) -> u64 {
            7
        }

        async fn list_pull_requests(&mut self) -> HandlerResult<Vec<SyncRequest>> {
            match self.listing_failure.take() {
                Some(error) => Err(error),
                None => Ok(self.pulls.clone()),
            }
        }

        async fn sync_pull_request(&mut self, request: &SyncRequest) -> Result<(), TerminalError> {
            self.synced.push(request.number);
            match self.sync_failures.remove(&request.number) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        async fn retain_pull_requests(&mut self, live: &[u64]) -> HandlerResult<()> {
            self.retained = Some(live.to_vec());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_failed_pull_request_sync_does_not_stop_the_sweep_or_the_cleanup() {
        let mut restate = RecordedRepoSync {
            pulls: vec![
                dependabot_pull(12),
                dependabot_pull(19),
                dependabot_pull(23),
            ],
            sync_failures: BTreeMap::from([(
                19,
                TerminalError::new("GitHub read failed with HTTP 404: Not Found"),
            )]),
            ..Default::default()
        };

        let outcome = run_repo_reconcile(&mut restate).await;

        assert_eq!(restate.synced, vec![12, 19, 23]);
        assert_eq!(
            restate.retained,
            Some(vec![12, 19, 23]),
            "the failed pull request is still open on GitHub, so its row must survive"
        );
        assert!(outcome.is_err(), "the failed sync stays visible to Restate");
    }

    #[tokio::test]
    async fn a_failed_listing_never_reaches_retention() {
        let mut restate = RecordedRepoSync {
            pulls: vec![dependabot_pull(12)],
            listing_failure: Some(
                TerminalError::new("GitHub read failed with HTTP 401: Bad credentials").into(),
            ),
            ..Default::default()
        };

        let outcome = run_repo_reconcile(&mut restate).await;

        assert!(outcome.is_err());
        assert!(restate.synced.is_empty());
        assert_eq!(
            restate.retained, None,
            "an unknown live set must not delete anything"
        );
    }

    #[tokio::test]
    async fn the_aggregate_failure_is_terminal_and_names_every_failed_pull_request() {
        let mut restate = RecordedRepoSync {
            pulls: vec![
                dependabot_pull(12),
                dependabot_pull(19),
                dependabot_pull(23),
            ],
            sync_failures: BTreeMap::from([
                (
                    12,
                    TerminalError::new("GitHub read failed with HTTP 404: Not Found"),
                ),
                (
                    23,
                    TerminalError::new("GitHub read failed with HTTP 410: Gone"),
                ),
            ]),
            ..Default::default()
        };

        let error = run_repo_reconcile(&mut restate).await.unwrap_err();

        let cause: &dyn std::error::Error = error.as_ref();
        assert_eq!(
            cause.to_string(),
            "Terminal error [500]: 2 of 3 pull request syncs failed while reconciling repository 7: 7#12, 7#23"
        );
    }

    #[tokio::test]
    async fn a_clean_sweep_retains_exactly_the_listed_pull_requests() {
        let mut restate = RecordedRepoSync {
            pulls: vec![dependabot_pull(12), dependabot_pull(19)],
            ..Default::default()
        };

        run_repo_reconcile(&mut restate).await.unwrap();

        assert_eq!(restate.synced, vec![12, 19]);
        assert_eq!(restate.retained, Some(vec![12, 19]));
    }

    #[tokio::test]
    async fn an_empty_listing_still_prunes_the_projection() {
        let mut restate = RecordedRepoSync::default();

        run_repo_reconcile(&mut restate).await.unwrap();

        assert_eq!(
            restate.retained,
            Some(vec![]),
            "every pull request closed means every stale row goes"
        );
    }

    #[test]
    fn scheduler_tick_accepts_legacy_empty_and_generation_inputs() {
        let mut legacy = Bytes::new();
        assert_eq!(
            <SchedulerTick as restate_sdk::serde::Deserialize>::deserialize(&mut legacy).unwrap(),
            SchedulerTick(None)
        );
        let mut current = Bytes::from_static(b"42");
        assert_eq!(
            <SchedulerTick as restate_sdk::serde::Deserialize>::deserialize(&mut current).unwrap(),
            SchedulerTick(Some(42))
        );
    }
}
