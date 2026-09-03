use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    net::SocketAddr,
    time::{Duration, Instant},
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
use restate_sdk::{filter::ReplayAwareFilter, prelude::*};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{net::TcpListener, signal};
use tracing::{debug, info, warn};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

const PR_STATE: &str = "pr_state";
const BATCH_PROGRESS: &str = "progress";
const SCHEDULER_STARTED: &str = "scheduler_started";
const SCHEDULER_GENERATION: &str = "scheduler_generation";
const SCHEDULER_TICK_PENDING: &str = "scheduler_tick_pending";
const DEFAULT_DEBOUNCE_SECONDS: u64 = 20;
const DEFAULT_RECONCILE_SECONDS: u64 = 60 * 60;
const MAX_CONCURRENT: usize = 3;
/// How many consecutive rate-limit waits one GitHub step honours before failing terminally.
///
/// Each wait runs until the deadline GitHub advertised (up to an hour for the primary
/// limit), so this bounds a chronically over-quota installation to a few hours per step
/// instead of an invisible, unbounded loop.
const MAX_RATE_LIMIT_WAITS: u32 = 3;

#[derive(Clone)]
struct PullRequest {
    github: GithubClient,
    store: LibSqlPrStore,
    debounce: Duration,
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

#[restate_sdk::object]
impl PullRequest {
    #[handler(ingress_private)]
    async fn sync(&self, ctx: ObjectContext<'_>, request: Json<SyncRequest>) -> HandlerResult<()> {
        traced("PullRequest/sync", ctx.key(), async {
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
                return Ok(SyncOutcome::Debounced);
            }
            state.sync_pending = false;
            ctx.set(PR_STATE, Json::from(state.clone()));

            let github = self.github.clone();
            let sync_request = request.clone();
            let fetched = run_github_step(&mut RestateGithubStep {
                ctx: &ctx,
                name: "fetch-canonical-pr-snapshot",
                operation: Operation::Read,
                known_resource: state.snapshot.is_some(),
                call: move || {
                    let github = github.clone();
                    let sync_request = sync_request.clone();
                    async move { github.fetch_snapshot(&sync_request).await }
                },
            })
            .await?;
            let snapshot = match fetched {
                Settled::Rejected {
                    reason: RejectReason::NotFound,
                    ..
                } => None,
                fetched => read_result(fetched)?,
            };

            let Some(snapshot) = snapshot else {
                let store = self.store.clone();
                let key = request_key(&request);
                ctx.run(move || async move {
                    store.delete_pr(&key).await.map_err(store_failure)?;
                    Ok(())
                })
                .retry_policy(store_retry_policy())
                .name("delete-ineligible-pr-projection")
                .await?;
                ctx.clear_all();
                return Ok(SyncOutcome::Deleted);
            };
            let store = self.store.clone();
            let projected = snapshot.clone();
            ctx.run(move || async move {
                store.upsert_pr(&projected).await.map_err(store_failure)?;
                Ok(())
            })
            .retry_policy(store_retry_policy())
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
            Ok(SyncOutcome::Synced {
                head_sha: snapshot.head_sha,
            })
        })
        .await
        .map(|_| ())
    }

    #[handler(ingress_private)]
    async fn closed(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        traced("PullRequest/closed", ctx.key(), async {
            let key = ctx
                .key()
                .parse::<PrKey>()
                .map_err(|error| TerminalError::new(error.to_string()))?;
            let store = self.store.clone();
            ctx.run(move || async move {
                store.delete_pr(&key).await.map_err(store_failure)?;
                Ok(())
            })
            .retry_policy(store_retry_policy())
            .name("delete-closed-pr-projection")
            .await?;
            ctx.clear_all();
            Ok(())
        })
        .await
    }

    #[handler(ingress_private)]
    async fn merge(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<MergeRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        traced("PullRequest/merge", ctx.key(), async {
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
            let result = run_github_step(&mut RestateGithubStep {
                ctx: &ctx,
                name: "merge-pull-request",
                operation: Operation::Merge,
                known_resource: false,
                call: move || {
                    let github = github.clone();
                    let merge_request = merge_request.clone();
                    async move { github.merge(&merge_request).await }
                },
            })
            .await?;
            let outcome = action_result(result)?;
            if matches!(outcome, ActionOutcome::Succeeded { .. }) {
                let store = self.store.clone();
                let key = request.target.key().parse::<PrKey>().map_err(|error| {
                    TerminalError::new(format!("invalid merge target key: {error}"))
                })?;
                ctx.run(move || async move {
                    store.delete_pr(&key).await.map_err(store_failure)?;
                    Ok(())
                })
                .retry_policy(store_retry_policy())
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
        })
        .await
    }

    #[handler(ingress_private)]
    async fn command(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<CommandRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        traced("PullRequest/command", ctx.key(), async {
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
            let result = run_github_step(&mut RestateGithubStep {
                ctx: &ctx,
                name: "post-dependabot-command",
                operation: Operation::Comment,
                known_resource: false,
                call: move || {
                    let github = github.clone();
                    let command_request = command_request.clone();
                    async move { github.post_command(&command_request).await }
                },
            })
            .await?;
            let outcome = action_result(result)?;
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
        })
        .await
    }

    #[handler(ingress_private)]
    async fn update_branch(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<UpdateBranchRequest>,
    ) -> HandlerResult<Json<ActionOutcome>> {
        traced("PullRequest/update_branch", ctx.key(), async {
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
            let result = run_github_step(&mut RestateGithubStep {
                ctx: &ctx,
                name: "update-pull-request-branch",
                operation: Operation::UpdateBranch,
                known_resource: false,
                call: move || {
                    let github = github.clone();
                    let update_request = update_request.clone();
                    async move { github.update_branch(&update_request).await }
                },
            })
            .await?;
            let outcome = action_result(result)?;
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
                        bypass_debounce: false,
                        completion_id: None,
                    }))
                    .send();
            }
            Ok(Json::from(outcome))
        })
        .await
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

struct BulkAction;

impl HandlerOutcome for Json<BatchProgress> {
    fn outcome(&self) -> String {
        let progress = &self.0;
        format!(
            "{} succeeded, {} rejected of {} targets",
            progress.succeeded,
            progress.rejected,
            progress.targets.len()
        )
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

/// What `InstallationSync.start` did, for its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchedulerStartOutcome {
    /// A tick for the current generation is already queued or delayed inside Restate.
    AlreadyArmed,
    Armed {
        generation: u64,
    },
}

impl HandlerOutcome for SchedulerStartOutcome {
    fn outcome(&self) -> String {
        match self {
            Self::AlreadyArmed => "already armed".to_owned(),
            Self::Armed { generation } => format!("armed generation {generation}"),
        }
    }
}

/// What `InstallationSync.tick` did, for its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SchedulerTickOutcome {
    /// The chain is paused or the tick is stale; nothing was touched.
    Dropped,
    Swept {
        repositories: usize,
    },
}

impl HandlerOutcome for SchedulerTickOutcome {
    fn outcome(&self) -> String {
        match self {
            Self::Dropped => "dropped".to_owned(),
            Self::Swept { repositories } => format!("swept {repositories} repositories"),
        }
    }
}

#[restate_sdk::object(ingress_private)]
impl InstallationSync {
    #[handler]
    async fn start(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        traced("InstallationSync/start", ctx.key(), async {
            let state = read_scheduler_state(&ctx).await?;
            let Some(next) = scheduler_start_transition(state)? else {
                return Ok(SchedulerStartOutcome::AlreadyArmed);
            };
            write_scheduler_state(&ctx, next);
            ctx.object_client::<InstallationSyncClient>(ctx.key())
                .tick(SchedulerTick(Some(next.generation)))
                .send();
            Ok(SchedulerStartOutcome::Armed {
                generation: next.generation,
            })
        })
        .await
        .map(|_| ())
    }

    #[handler]
    async fn tick(&self, ctx: ObjectContext<'_>, generation: SchedulerTick) -> HandlerResult<()> {
        traced("InstallationSync/tick", ctx.key(), async {
            let state = read_scheduler_state(&ctx).await?;
            let mut restate = RestateTickEffects {
                ctx: &ctx,
                github: &self.github,
                store: &self.store,
                interval: self.interval,
            };
            run_scheduler_tick(&mut restate, state, generation).await
        })
        .await
        .map(|_| ())
    }

    #[handler]
    async fn sync_now(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        traced("InstallationSync/sync_now", ctx.key(), async {
            let repositories =
                perform_installation_sync(&ctx, self.github.clone(), self.store.clone()).await?;
            Ok(format!("fanned out to {repositories} repositories"))
        })
        .await
        .map(|_| ())
    }

    #[handler]
    async fn pause(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        traced("InstallationSync/pause", ctx.key(), async {
            ctx.clear(SCHEDULER_STARTED);
            ctx.clear(SCHEDULER_TICK_PENDING);
            invalidate_scheduler_generation(&ctx).await?;
            Ok(())
        })
        .await
    }

    #[handler]
    async fn purge(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        traced("InstallationSync/purge", ctx.key(), async {
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
            .retry_policy(store_retry_policy())
            .name("purge-installation")
            .await?;
            Ok(())
        })
        .await
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
    fn persist(&mut self, state: SchedulerState);
    fn schedule_tick(&mut self, generation: u64);
    /// Sweeps the installation; resolves to how many repositories were fanned out to.
    fn sweep(&mut self) -> impl Future<Output = HandlerResult<usize>> + Send;
}

struct RestateTickEffects<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    github: &'a GithubClient,
    store: &'a LibSqlPrStore,
    interval: Duration,
}

impl SchedulerTickEffects for RestateTickEffects<'_, '_> {
    fn persist(&mut self, state: SchedulerState) {
        write_scheduler_state(self.ctx, state);
    }

    fn schedule_tick(&mut self, generation: u64) {
        self.ctx
            .object_client::<InstallationSyncClient>(self.ctx.key())
            .tick(SchedulerTick(Some(generation)))
            .send_after(self.interval);
    }

    async fn sweep(&mut self) -> HandlerResult<usize> {
        perform_installation_sync(self.ctx, self.github.clone(), self.store.clone()).await
    }
}

async fn run_scheduler_tick<E: SchedulerTickEffects>(
    restate: &mut E,
    state: SchedulerState,
    incoming: SchedulerTick,
) -> HandlerResult<SchedulerTickOutcome> {
    let Some(next) = scheduler_tick_transition(state, incoming)? else {
        return Ok(SchedulerTickOutcome::Dropped);
    };
    // Re-arm before sweeping. Restate never rolls back journaled state or sends, so the
    // chain survives a terminal or aborted sweep instead of dying silently; the failure
    // itself surfaces as the handler's, logged by `traced` with the installation id.
    restate.persist(next);
    restate.schedule_tick(next.generation);
    let repositories = restate.sweep().await?;
    Ok(SchedulerTickOutcome::Swept { repositories })
}

async fn invalidate_scheduler_generation(ctx: &ObjectContext<'_>) -> HandlerResult<()> {
    let generation =
        next_scheduler_generation(ctx.get::<u64>(SCHEDULER_GENERATION).await?.unwrap_or(0))?;
    ctx.set(SCHEDULER_GENERATION, generation);
    Ok(())
}

/// Re-enumerates the installation's repositories and fans a reconcile out to each; resolves
/// to how many.
async fn perform_installation_sync(
    ctx: &ObjectContext<'_>,
    github: GithubClient,
    store: LibSqlPrStore,
) -> HandlerResult<usize> {
    let reconcile_start = ctx
        .run(|| async { Ok(unix_seconds()) })
        .name("installation-reconcile-clock")
        .await?;
    let list_client = github.clone();
    let repositories = run_github_step(&mut RestateGithubStep {
        ctx,
        name: "list-installation-repositories",
        operation: Operation::Read,
        known_resource: false,
        call: move || {
            let github = list_client.clone();
            async move { github.list_installation_repositories().await }
        },
    })
    .await?;
    let repositories = read_result(repositories)?;
    let stored = repositories.clone();
    let installation_id = github.installation_id();
    ctx.run(move || async move {
        store
            .replace_installation_repos(installation_id, &stored, reconcile_start)
            .await
            .map_err(store_failure)?;
        Ok(())
    })
    .retry_policy(store_retry_policy())
    .name("replace-installation-repositories")
    .await?;
    let count = repositories.len();
    for repository in repositories {
        ctx.object_client::<RepoSyncClient>(repository.repository_id.to_string())
            .reconcile(Json::from(repository))
            .send();
    }
    Ok(count)
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
        let pulls = run_github_step(&mut RestateGithubStep {
            ctx: self.ctx,
            name: "list-open-dependabot-prs",
            operation: Operation::Read,
            known_resource: false,
            call: move || {
                let github = github.clone();
                let owner = owner.clone();
                let repo = repo.clone();
                async move {
                    github
                        .list_dependabot_prs(&owner, &repo, repository_id)
                        .await
                }
            },
        })
        .await?;
        read_result(pulls)
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
            .retry_policy(store_retry_policy())
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
/// live set is unknown and nothing is deleted. Resolves to how many pull requests synced.
async fn run_repo_reconcile<E: RepoReconcileEffects>(restate: &mut E) -> HandlerResult<usize> {
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
        return Ok(pulls.len());
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
        traced("RepoSync/reconcile", ctx.key(), async {
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
            let pull_requests = run_repo_reconcile(&mut restate).await?;
            Ok(format!("synced {pull_requests} pull requests"))
        })
        .await
        .map(|_| ())
    }

    #[handler]
    async fn sync_sha(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<SyncShaRequest>,
    ) -> HandlerResult<()> {
        traced("RepoSync/sync_sha", ctx.key(), async {
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
                .retry_policy(store_retry_policy())
                .name("resolve-prs-for-sha")
                .await?;
            let mut numbers = request
                .pull_requests
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            numbers.extend(matches.into_inner().into_iter().map(|pull| pull.number));
            let count = numbers.len();
            for number in numbers {
                let sync = SyncRequest {
                    repository_id: request.repository_id,
                    owner: request.owner.clone(),
                    repo: request.repo.clone(),
                    number,
                    bypass_debounce: false,
                    completion_id: None,
                };
                ctx.object_client::<PullRequestClient>(request_key(&sync).to_string())
                    .sync(Json::from(sync))
                    .send();
            }
            Ok(format!(
                "fanned out to {count} pull requests at {}",
                short_sha(&request.sha)
            ))
        })
        .await
        .map(|_| ())
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
        let installation_id = self.installation_id.to_string();
        traced("SchedulerIngress/start", &installation_id, async {
            ctx.object_client::<InstallationSyncClient>(installation_id.clone())
                .start()
                .send();
            Ok(())
        })
        .await
    }
}

#[restate_sdk::service]
impl WebhookIngress {
    /// Routes a verified GitHub delivery to the object that owns it.
    ///
    /// The web edge acknowledges kinds with no arm in `route_webhook` before they reach
    /// Restate, and its copy of this table (`route_delivery` in the web app)
    /// must be kept in step: adding a kind to `route_webhook` without adding it there
    /// means the edge silently swallows it.
    #[handler]
    async fn dispatch(&self, ctx: Context<'_>, event: Json<WebhookEvent>) -> HandlerResult<()> {
        let event = event.into_inner();
        // A service has no object key; the delivery kind is what identifies this invocation.
        let delivery = format!("{}.{}", event.event, event.action.as_deref().unwrap_or("-"));
        traced("WebhookIngress/dispatch", &delivery, async {
            // A rejection is the handler's failure, so `traced` logs it with its reason;
            // the message carries the ids needed to find the delivery in GitHub.
            let route = route_webhook(self.installation_id, &event)
                .map_err(|rejection| TerminalError::new(rejection.to_string()))?;
            send_webhook_route(&ctx, &route);
            Ok(route)
        })
        .await
        .map(|_| ())
    }
}

/// Where a webhook delivery goes, decided before anything is sent so the decision can be
/// tested and logged without a Restate context.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WebhookRoute {
    ClosePullRequest(PrKey),
    SyncPullRequest(SyncRequest),
    SyncSha(SyncShaRequest),
    Installation {
        installation_id: u64,
        action: InstallationLifecycleAction,
    },
    /// A routed kind whose action has no arm: `check_suite.requested`,
    /// `check_run.rerequested`, `pull_request.assigned`, and the like. Kinds the edge
    /// acknowledges never get this far.
    Ignore,
}

impl HandlerOutcome for WebhookRoute {
    fn outcome(&self) -> String {
        match self {
            Self::ClosePullRequest(key) => format!("sent PullRequest/{key}.closed"),
            Self::SyncPullRequest(request) => {
                format!("sent PullRequest/{}.sync", request_key(request))
            }
            Self::SyncSha(request) => {
                format!("sent RepoSync/{}.sync_sha", request.repository_id)
            }
            Self::Installation {
                installation_id,
                action,
            } => {
                let handler = match action {
                    InstallationLifecycleAction::Start => "start",
                    InstallationLifecycleAction::SyncNow => "sync_now",
                    InstallationLifecycleAction::Pause => "pause",
                    InstallationLifecycleAction::Purge => "purge",
                };
                format!("sent InstallationSync/{installation_id}.{handler}")
            }
            Self::Ignore => "ignored".to_owned(),
        }
    }
}

/// Why a delivery can never be routed, however often Restate retried it.
///
/// Rendered into the terminal failure and the handler's log line, so it must carry the ids
/// an operator needs to find the offending delivery in GitHub.
#[derive(Debug, Error, PartialEq, Eq)]
enum WebhookRejection {
    #[error("webhook installation {actual} does not match configured installation {expected}")]
    ForeignInstallation { expected: u64, actual: u64 },
    #[error("{kind} webhook is missing {}", fields.join(", "))]
    MissingFields {
        kind: &'static str,
        fields: Vec<&'static str>,
    },
}

fn route_webhook(
    installation_id: u64,
    event: &WebhookEvent,
) -> Result<WebhookRoute, WebhookRejection> {
    if let Some(actual) = event
        .installation_id
        .filter(|actual| *actual != installation_id)
    {
        return Err(WebhookRejection::ForeignInstallation {
            expected: installation_id,
            actual,
        });
    }
    match event.event.as_str() {
        "pull_request" => route_pull_request(event),
        "check_suite" if event.action.as_deref() == Some("completed") => route_sha(event),
        "check_run" if matches!(event.action.as_deref(), Some("created" | "completed")) => {
            route_sha(event)
        }
        "status" => route_sha(event),
        "installation" => route_installation(event),
        "installation_repositories" => Ok(match event.installation_id {
            Some(installation_id) => WebhookRoute::Installation {
                installation_id,
                action: InstallationLifecycleAction::SyncNow,
            },
            None => WebhookRoute::Ignore,
        }),
        _ => Ok(WebhookRoute::Ignore),
    }
}

fn route_pull_request(event: &WebhookEvent) -> Result<WebhookRoute, WebhookRejection> {
    let (Some(repository_id), Some(owner), Some(repo), Some(number)) = (
        event.repository_id,
        event.owner.as_ref(),
        event.repo.as_ref(),
        event.number,
    ) else {
        return Err(missing_fields(
            "pull_request",
            [
                ("repository_id", event.repository_id.is_none()),
                ("owner", event.owner.is_none()),
                ("repo", event.repo.is_none()),
                ("number", event.number.is_none()),
            ],
        ));
    };
    Ok(match event.action.as_deref() {
        Some("closed") => WebhookRoute::ClosePullRequest(PrKey::new(repository_id, number)),
        action if pull_request_action_requests_sync(action) => {
            WebhookRoute::SyncPullRequest(SyncRequest {
                repository_id,
                owner: owner.clone(),
                repo: repo.clone(),
                number,
                bypass_debounce: action == Some(DASHBOARD_SYNC_ACTION),
                completion_id: event.sync_completion_id.clone(),
            })
        }
        _ => WebhookRoute::Ignore,
    })
}

fn route_sha(event: &WebhookEvent) -> Result<WebhookRoute, WebhookRejection> {
    let (Some(repository_id), Some(owner), Some(repo), Some(sha)) = (
        event.repository_id,
        event.owner.as_ref(),
        event.repo.as_ref(),
        event.sha.as_ref(),
    ) else {
        return Err(missing_fields(
            "commit",
            [
                ("repository_id", event.repository_id.is_none()),
                ("owner", event.owner.is_none()),
                ("repo", event.repo.is_none()),
                ("sha", event.sha.is_none()),
            ],
        ));
    };
    Ok(WebhookRoute::SyncSha(SyncShaRequest {
        repository_id,
        owner: owner.clone(),
        repo: repo.clone(),
        sha: sha.clone(),
        pull_requests: event.pull_requests.clone(),
    }))
}

fn route_installation(event: &WebhookEvent) -> Result<WebhookRoute, WebhookRejection> {
    let Some(installation_id) = event.installation_id else {
        return Err(missing_fields("installation", [("installation_id", true)]));
    };
    Ok(
        match installation_lifecycle_action(event.action.as_deref()) {
            Some(action) => WebhookRoute::Installation {
                installation_id,
                action,
            },
            None => WebhookRoute::Ignore,
        },
    )
}

fn missing_fields(
    kind: &'static str,
    fields: impl IntoIterator<Item = (&'static str, bool)>,
) -> WebhookRejection {
    WebhookRejection::MissingFields {
        kind,
        fields: fields
            .into_iter()
            .filter_map(|(field, missing)| missing.then_some(field))
            .collect(),
    }
}

fn send_webhook_route(ctx: &Context<'_>, route: &WebhookRoute) {
    match route {
        WebhookRoute::ClosePullRequest(key) => {
            ctx.object_client::<PullRequestClient>(key.to_string())
                .closed()
                .send();
        }
        WebhookRoute::SyncPullRequest(request) => {
            ctx.object_client::<PullRequestClient>(request_key(request).to_string())
                .sync(Json::from(request.clone()))
                .send();
        }
        WebhookRoute::SyncSha(request) => {
            ctx.object_client::<RepoSyncClient>(request.repository_id.to_string())
                .sync_sha(Json::from(request.clone()))
                .send();
        }
        WebhookRoute::Installation {
            installation_id,
            action,
        } => {
            let client = ctx.object_client::<InstallationSyncClient>(installation_id.to_string());
            match action {
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
            }
        }
        WebhookRoute::Ignore => {}
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallationLifecycleAction {
    Start,
    SyncNow,
    Pause,
    Purge,
}

fn installation_lifecycle_action(action: Option<&str>) -> Option<InstallationLifecycleAction> {
    match action {
        Some("created") => Some(InstallationLifecycleAction::SyncNow),
        Some("unsuspend") => Some(InstallationLifecycleAction::Start),
        Some("suspend") => Some(InstallationLifecycleAction::Pause),
        Some("deleted") => Some(InstallationLifecycleAction::Purge),
        _ => None,
    }
}

/// What one attempt of a journaled GitHub step records.
///
/// The error classification is computed inside the `ctx.run` closure and journaled here, so
/// replay interprets the recorded classification instead of recomputing it. Transient
/// failures are not journaled at all: they fail the attempt so the step's bounded retry
/// policy re-runs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Attempt<T> {
    Settled(Settled<T>),
    /// GitHub asked us to back off until `until` (unix seconds). The handler sleeps durably
    /// and re-runs the step; nothing waits inside the journaled closure.
    RateLimited {
        until: u64,
        response: GithubErrorResponse,
    },
}

/// The journaled result of a GitHub step once every wait and retry has been honoured.
///
/// Failures keep the raw response next to their classification so the journal entry in
/// Restate shows what GitHub actually said, not just what we made of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Settled<T> {
    Ok(T),
    Rejected {
        reason: RejectReason,
        response: GithubErrorResponse,
    },
    Fatal {
        response: GithubErrorResponse,
    },
    StaleSha {
        expected: String,
        actual: String,
    },
}

/// Maps a GitHub call result to what the `ctx.run` closure journals.
///
/// `now` is the closure's clock reading, so the rate-limit deadline is fixed once and
/// replays verbatim. Runs inside the journaled closure; must never block.
fn journal_github_result<T>(
    result: Result<T, GithubError>,
    operation: Operation,
    known_resource: bool,
    now: u64,
) -> HandlerResult<Json<Attempt<T>>>
where
    T: Serialize + for<'de> Deserialize<'de> + 'static,
{
    let (response, known_resource) = match result {
        Ok(value) => return Ok(Json::from(Attempt::Settled(Settled::Ok(value)))),
        Err(GithubError::Http(response)) => (response, known_resource),
        Err(GithubError::HttpKnown(response)) => (response, true),
        Err(GithubError::StaleSha { expected, actual }) => {
            return Ok(Json::from(Attempt::Settled(Settled::StaleSha {
                expected,
                actual,
            })));
        }
        Err(GithubError::Transport(message)) => {
            return Err(RetryableServiceError::Github(message).into());
        }
        Err(GithubError::Protocol(message) | GithubError::Config(message)) => {
            return Err(TerminalError::new(message).into());
        }
    };
    let attempt = match classify_github_error(&response, operation, known_resource, now) {
        Classification::Retryable => {
            return Err(RetryableServiceError::Github(response.message).into());
        }
        Classification::RateLimited { until } => Attempt::RateLimited { until, response },
        Classification::Rejected(reason) => {
            Attempt::Settled(Settled::Rejected { reason, response })
        }
        Classification::Fatal => Attempt::Settled(Settled::Fatal { response }),
    };
    Ok(Json::from(attempt))
}

/// One journaled GitHub step and the durable wait a rate limit asks of Restate, abstracted
/// so `run_github_step` can be exercised against a recording fake without a runtime.
trait GithubStepEffects<T> {
    fn name(&self) -> &str;
    /// Runs the step once inside `ctx.run`; transient failures are retried by the step's
    /// bounded retry policy and only surface here once that budget is exhausted.
    fn attempt(&mut self) -> impl Future<Output = Result<Attempt<T>, TerminalError>> + Send;
    /// Sleeps durably until `until` (unix seconds).
    fn sleep_until(&mut self, until: u64)
    -> impl Future<Output = Result<(), TerminalError>> + Send;
}

struct RestateGithubStep<'a, 'ctx, F> {
    ctx: &'a ObjectContext<'ctx>,
    name: &'static str,
    operation: Operation,
    known_resource: bool,
    call: F,
}

impl<T, F, Fut> GithubStepEffects<T> for RestateGithubStep<'_, '_, F>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static,
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<T, GithubError>> + Send + 'static,
{
    fn name(&self) -> &str {
        self.name
    }

    async fn attempt(&mut self) -> Result<Attempt<T>, TerminalError> {
        let call = self.call.clone();
        let operation = self.operation;
        let known_resource = self.known_resource;
        self.ctx
            .run(move || async move {
                journal_github_result(call().await, operation, known_resource, unix_seconds())
            })
            .retry_policy(github_retry_policy())
            .name(self.name)
            .await
            .map(Json::into_inner)
    }

    async fn sleep_until(&mut self, until: u64) -> Result<(), TerminalError> {
        // The sleep entry journals its own wake-up time and replay matches it by position,
        // not by duration, so this clock reading needs no journaling of its own.
        self.ctx
            .sleep(Duration::from_secs(until.saturating_sub(unix_seconds())))
            .await
    }
}

/// Runs a GitHub call as a journaled step until it settles.
///
/// A rate-limited attempt sleeps durably until the advertised deadline and re-runs the
/// step; the wait is a journal entry, visible in Restate, and survives a process restart.
/// A step that is still rate limited after `MAX_RATE_LIMIT_WAITS` waits fails terminally
/// rather than looping out of sight.
async fn run_github_step<T, E: GithubStepEffects<T>>(step: &mut E) -> HandlerResult<Settled<T>> {
    let mut waits = 0;
    loop {
        let (until, response) = match step.attempt().await? {
            Attempt::Settled(settled) => return Ok(settled),
            Attempt::RateLimited { until, response } => (until, response),
        };
        if waits == MAX_RATE_LIMIT_WAITS {
            let name = step.name();
            let message = response.message;
            return Err(TerminalError::new(format!(
                "GitHub kept rate limiting {name} after {MAX_RATE_LIMIT_WAITS} waits; last reset at {until}: {message}"
            ))
            .into());
        }
        waits += 1;
        info!(
            step = step.name(),
            until,
            wait = waits,
            message = %response.message,
            "GitHub rate limited; sleeping durably until the limit resets"
        );
        step.sleep_until(until).await?;
    }
}

/// Bounded backoff for GitHub calls: transient failures (5xx, transport) back off from one
/// second to five minutes and give up after thirty minutes with a terminal failure.
fn github_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5 * 60))
        .max_duration(Duration::from_secs(30 * 60))
}

/// Bounded backoff for projection-store writes: SQLite contention and the FK race between
/// a fresh repository's first webhook and its enumeration resolve within seconds, so back
/// off from 100ms to five seconds and give up after five minutes with a terminal failure.
/// A webhook that outlives the budget is not lost: the next reconcile syncs the same pull
/// request once its repository row exists.
fn store_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(100))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5))
        .max_duration(Duration::from_secs(5 * 60))
}

fn store_failure(error: StoreError) -> HandlerError {
    match error.class() {
        StoreErrorClass::Retryable => RetryableServiceError::Store(error.to_string()).into(),
        StoreErrorClass::Terminal => TerminalError::new(error.to_string()).into(),
    }
}

fn read_result<T>(result: Settled<T>) -> HandlerResult<T> {
    match result {
        Settled::Ok(value) => Ok(value),
        Settled::Rejected { reason, .. } => Err(TerminalError::new(reason.to_string()).into()),
        Settled::Fatal { response } => Err(TerminalError::new(format!(
            "GitHub read failed with HTTP {}: {}",
            response.status, response.message
        ))
        .into()),
        Settled::StaleSha { .. } => {
            Err(TerminalError::new("unexpected stale SHA while reading GitHub").into())
        }
    }
}

fn action_result(result: Settled<String>) -> HandlerResult<ActionOutcome> {
    match result {
        Settled::Ok(detail) => Ok(ActionOutcome::Succeeded { detail }),
        Settled::StaleSha { expected, actual } => {
            Ok(rejected(RejectReason::StaleSha { expected, actual }))
        }
        Settled::Rejected { reason, .. } => Ok(rejected(reason)),
        Settled::Fatal { response } => Err(TerminalError::new(format!(
            "GitHub mutation failed with HTTP {}: {}",
            response.status, response.message
        ))
        .into()),
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

/// A failure worth retrying inside a `ctx.run`. Once the step's retry budget is exhausted the
/// SDK surfaces this message as the terminal failure, so it must read well on its own.
#[derive(Debug, Error)]
enum RetryableServiceError {
    #[error("transient GitHub failure: {0}")]
    Github(String),
    #[error("transient projection-store failure: {0}")]
    Store(String),
}

/// How a handler's successful result reads in its completion log line.
trait HandlerOutcome {
    fn outcome(&self) -> String;
}

impl HandlerOutcome for () {
    fn outcome(&self) -> String {
        "ok".to_owned()
    }
}

/// For handlers whose only interesting result is a count, e.g. how many objects they
/// fanned out to; the handler phrases it.
impl HandlerOutcome for String {
    fn outcome(&self) -> String {
        self.clone()
    }
}

impl HandlerOutcome for Json<ActionOutcome> {
    fn outcome(&self) -> String {
        match &self.0 {
            ActionOutcome::Succeeded { .. } => "succeeded".to_owned(),
            ActionOutcome::Rejected { reason } => format!("rejected: {reason}"),
        }
    }
}

impl<T> HandlerOutcome for Json<Option<T>> {
    fn outcome(&self) -> String {
        match &self.0 {
            Some(_) => "found".to_owned(),
            None => "absent".to_owned(),
        }
    }
}

/// Runs one handler attempt and logs how it ended, with the object key and elapsed time.
///
/// Called from inside the handler so the line carries the key; the SDK's span already names
/// the service and method. An attempt that suspends and replays logs once, when it really
/// finishes; an attempt that fails retryably logs each failure. `elapsed_ms` is this
/// attempt's wall time, not the invocation's age.
async fn traced<T: HandlerOutcome>(
    handler: &'static str,
    key: &str,
    attempt: impl Future<Output = HandlerResult<T>>,
) -> HandlerResult<T> {
    let (result, elapsed_ms) = timed(attempt).await;
    match &result {
        Ok(value) => info!(
            handler,
            key,
            outcome = value.outcome().as_str(),
            elapsed_ms,
            "handler completed"
        ),
        Err(error) => log_handler_failure(handler, key, error, elapsed_ms),
    }
    result
}

/// `traced` for the read-only handlers the dashboard polls: success is `debug`, so a busy
/// dashboard cannot drown the write path in the log.
async fn traced_read<T: HandlerOutcome>(
    handler: &'static str,
    key: &str,
    attempt: impl Future<Output = HandlerResult<T>>,
) -> HandlerResult<T> {
    let (result, elapsed_ms) = timed(attempt).await;
    match &result {
        Ok(value) => debug!(
            handler,
            key,
            outcome = value.outcome().as_str(),
            elapsed_ms,
            "handler completed"
        ),
        Err(error) => log_handler_failure(handler, key, error, elapsed_ms),
    }
    result
}

async fn timed<T>(attempt: impl Future<Output = T>) -> (T, u128) {
    let started = Instant::now();
    let result = attempt.await;
    (result, started.elapsed().as_millis())
}

fn log_handler_failure(handler: &'static str, key: &str, error: &HandlerError, elapsed_ms: u128) {
    warn!(
        handler,
        key,
        outcome = "failed",
        cause = %handler_cause(error),
        elapsed_ms,
        "handler failed"
    );
}

/// `HandlerError` only renders through `AsRef<dyn Error>`; its message already says whether
/// Restate will treat the failure as terminal or retry it.
fn handler_cause(error: &HandlerError) -> &dyn std::error::Error {
    error.as_ref()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Restate re-runs handler code while replaying a journal, so without the replay filter
    // every line a handler logged before it suspended would be logged again on resume.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
                .with_filter(ReplayAwareFilter),
        )
        .init();

    let store = LibSqlPrStore::connect(&StoreConfig::from_env()).await?;
    let github = GithubClient::new(GithubConfig::from_env()?)?;
    let debounce = Duration::from_secs(env_seconds(
        "SYNC_DEBOUNCE_SECONDS",
        DEFAULT_DEBOUNCE_SECONDS,
    ));
    let interval = Duration::from_secs(env_seconds(
        "RECONCILE_INTERVAL_SECONDS",
        DEFAULT_RECONCILE_SECONDS,
    ));
    let installation_id = github.installation_id();
    info!(
        installation_id,
        debounce_seconds = debounce.as_secs(),
        reconcile_interval_seconds = interval.as_secs(),
        "resolved service settings"
    );

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
    let address: SocketAddr = env::var("RESTATE_SERVICE_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:9080".to_owned())
        .parse()?;
    let listener = TcpListener::bind(address).await?;
    info!(%address, "starting Restate service endpoint");
    HttpServer::new(endpoint)
        .serve_with_cancel(listener, shutdown_signal())
        .await;
    info!("Restate service endpoint stopped");
    Ok(())
}

/// Resolves once the process is asked to stop, by Ctrl-C or by its supervisor.
///
/// The SDK only listens for SIGINT, but containers stop with SIGTERM; without this arm a
/// `docker stop` kills the process mid-step and every in-flight invocation has to be
/// replayed. Either signal starts the SDK's graceful drain: the listener closes, open
/// invocations get up to ten seconds to finish, and Restate retries whatever is left.
async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = signal::ctrl_c().await {
            warn!(%error, "cannot listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(error) => {
                warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let signal = tokio::select! {
        () = interrupt => "SIGINT",
        () = terminate => "SIGTERM",
    };
    info!(signal, "shutdown requested; draining in-flight invocations");
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

/// Reads a whole-seconds setting from the environment.
fn env_seconds(name: &str, default: u64) -> u64 {
    let raw = match env::var(name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        // Not UTF-8 cannot be a number either; surface it as the typo it is.
        Err(env::VarError::NotUnicode(raw)) => Some(raw.to_string_lossy().into_owned()),
    };
    resolve_seconds(name, raw.as_deref(), default)
}

/// Resolves a whole-seconds setting from its raw environment value.
///
/// A typo must not silently become the default: an unparsable value still falls back, but
/// says so and names the variable, so the operator learns at startup rather than from the
/// service's behaviour. Unset or empty means "use the default" and is not worth a line.
fn resolve_seconds(name: &str, raw: Option<&str>, default: u64) -> u64 {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return default;
    };
    match raw.parse::<u64>() {
        Ok(seconds) => seconds,
        Err(error) => {
            warn!(
                variable = name,
                value = raw,
                default,
                %error,
                "environment setting is not a whole number of seconds; using the default"
            );
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use dependaboard_core::{CheckStatus, Mergeable, PrRecord, PrTarget, UpdateType, UserId};
    use restate_sdk::service::Discoverable;
    use tracing::instrument::WithSubscriber;

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

    fn is_retryable(error: &HandlerError) -> bool {
        let cause: &dyn std::error::Error = error.as_ref();
        cause.to_string().starts_with("Retryable error")
    }

    fn is_terminal(error: &HandlerError) -> bool {
        let cause: &dyn std::error::Error = error.as_ref();
        cause.to_string().starts_with("Terminal error")
    }

    /// Journals a failed GitHub call at `now = 1_000` and returns the attempt's failure.
    fn journal_failure(error: GithubError, operation: Operation) -> HandlerError {
        match journal_github_result::<()>(Err(error), operation, false, 1_000) {
            Err(error) => error,
            Ok(attempt) => panic!(
                "expected the attempt to fail, got {:?}",
                attempt.into_inner()
            ),
        }
    }

    #[test]
    fn a_rate_limited_response_is_journaled_with_its_deadline() {
        let response = GithubErrorResponse {
            status: 403,
            message: "You have exceeded a secondary rate limit.".to_owned(),
            retry_after_seconds: Some(45),
            ..Default::default()
        };

        let attempt = journal_github_result::<()>(
            Err(GithubError::Http(response.clone())),
            Operation::Comment,
            false,
            1_000,
        )
        .expect("a rate limit is a journaled value, not a run failure")
        .into_inner();

        assert!(matches!(
            attempt,
            Attempt::RateLimited { until: 1_045, response: journaled } if journaled == response
        ));
    }

    #[test]
    fn a_transient_failure_fails_the_attempt_so_the_bounded_policy_retries_it() {
        let unavailable = GithubErrorResponse {
            status: 503,
            message: "unavailable".to_owned(),
            ..Default::default()
        };
        let error = journal_failure(GithubError::Http(unavailable), Operation::Read);
        assert!(is_retryable(&error), "{error:?}");

        let error = journal_failure(
            GithubError::Transport("connection reset".to_owned()),
            Operation::Read,
        );
        assert!(is_retryable(&error), "{error:?}");
    }

    #[test]
    fn a_rejected_response_is_journaled_with_its_reason() {
        let response = GithubErrorResponse {
            status: 404,
            message: "Not Found".to_owned(),
            ..Default::default()
        };

        let attempt = journal_github_result::<()>(
            Err(GithubError::HttpKnown(response.clone())),
            Operation::Read,
            false,
            1_000,
        )
        .unwrap()
        .into_inner();

        assert_eq!(
            attempt,
            Attempt::Settled(Settled::Rejected {
                reason: RejectReason::NotFound,
                response,
            })
        );
    }

    #[test]
    fn a_fatal_response_is_journaled_verbatim() {
        let response = GithubErrorResponse {
            status: 401,
            message: "Bad credentials".to_owned(),
            ..Default::default()
        };
        let attempt = journal_github_result::<()>(
            Err(GithubError::Http(response.clone())),
            Operation::Read,
            false,
            1_000,
        )
        .unwrap()
        .into_inner();
        assert_eq!(attempt, Attempt::Settled(Settled::Fatal { response }));
    }

    #[test]
    fn a_protocol_error_fails_the_attempt_terminally() {
        let error = journal_failure(
            GithubError::Protocol("invalid GitHub response".to_owned()),
            Operation::Read,
        );
        assert!(is_terminal(&error), "{error:?}");
    }

    /// Stands in for one journaled GitHub step and records the durable waits it asked of
    /// Restate.
    struct RecordedGithubStep {
        attempts: VecDeque<Attempt<String>>,
        slept_until: Vec<u64>,
    }

    impl RecordedGithubStep {
        fn new(attempts: impl IntoIterator<Item = Attempt<String>>) -> Self {
            Self {
                attempts: attempts.into_iter().collect(),
                slept_until: Vec::new(),
            }
        }
    }

    fn rate_limited(until: u64) -> Attempt<String> {
        Attempt::RateLimited {
            until,
            response: GithubErrorResponse {
                status: 403,
                message: "API rate limit exceeded".to_owned(),
                rate_limit_remaining: Some(0),
                rate_limit_reset: Some(until),
                ..Default::default()
            },
        }
    }

    impl GithubStepEffects<String> for RecordedGithubStep {
        fn name(&self) -> &str {
            "merge-pull-request"
        }

        async fn attempt(&mut self) -> Result<Attempt<String>, TerminalError> {
            Ok(self
                .attempts
                .pop_front()
                .expect("the step ran more attempts than the test scripted"))
        }

        async fn sleep_until(&mut self, until: u64) -> Result<(), TerminalError> {
            self.slept_until.push(until);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_rate_limited_attempt_sleeps_durably_until_the_deadline_then_retries() {
        let mut step = RecordedGithubStep::new([
            rate_limited(4_600),
            Attempt::Settled(Settled::Ok("merged".to_owned())),
        ]);

        let settled = run_github_step(&mut step).await.unwrap();

        assert_eq!(settled, Settled::Ok("merged".to_owned()));
        assert_eq!(step.slept_until, vec![4_600]);
        assert!(
            step.attempts.is_empty(),
            "the step was re-run after the wait"
        );
    }

    #[tokio::test]
    async fn a_step_that_stays_rate_limited_gives_up_terminally_after_three_waits() {
        let mut step = RecordedGithubStep::new([
            rate_limited(4_600),
            rate_limited(8_200),
            rate_limited(11_800),
            rate_limited(15_400),
        ]);

        let error = run_github_step(&mut step).await.unwrap_err();

        assert_eq!(step.slept_until, vec![4_600, 8_200, 11_800]);
        let cause: &dyn std::error::Error = error.as_ref();
        assert_eq!(
            cause.to_string(),
            "Terminal error [500]: GitHub kept rate limiting merge-pull-request after 3 waits; last reset at 15400: API rate limit exceeded"
        );
    }

    #[tokio::test]
    async fn a_settled_attempt_never_waits() {
        let mut step = RecordedGithubStep::new([Attempt::Settled(Settled::Rejected {
            reason: RejectReason::NotMergeable,
            response: GithubErrorResponse {
                status: 405,
                message: "Pull Request is not mergeable".to_owned(),
                ..Default::default()
            },
        })]);

        let settled = run_github_step(&mut step).await.unwrap();

        assert!(matches!(
            settled,
            Settled::Rejected {
                reason: RejectReason::NotMergeable,
                ..
            }
        ));
        assert!(step.slept_until.is_empty());
    }

    #[test]
    fn every_run_retry_policy_gives_up_after_a_bounded_duration() {
        // The SDK exposes no accessor for the policy's bounds, so inspect its Debug form.
        let github = format!("{:?}", github_retry_policy());
        assert!(github.contains("max_duration: Some(1800s)"), "{github}");
        let store = format!("{:?}", store_retry_policy());
        assert!(store.contains("max_duration: Some(300s)"), "{store}");
    }

    fn forbidden() -> GithubErrorResponse {
        GithubErrorResponse {
            status: 403,
            message: "Resource not accessible by integration".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn settled_mutations_map_to_outcomes_and_only_fatal_ones_fail() {
        assert_eq!(
            action_result(Settled::Rejected {
                reason: RejectReason::Forbidden,
                response: forbidden(),
            })
            .unwrap(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden
            }
        );
        assert_eq!(
            action_result(Settled::Ok("merged".to_owned())).unwrap(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned()
            }
        );

        let error = action_result(Settled::Fatal {
            response: GithubErrorResponse {
                status: 401,
                message: "Bad credentials".to_owned(),
                ..Default::default()
            },
        })
        .unwrap_err();
        let cause: &dyn std::error::Error = error.as_ref();
        assert_eq!(
            cause.to_string(),
            "Terminal error [500]: GitHub mutation failed with HTTP 401: Bad credentials"
        );
    }

    #[test]
    fn a_rejected_read_has_no_outcome_to_carry_it_and_fails_terminally() {
        let error = read_result(Settled::<()>::Rejected {
            reason: RejectReason::Forbidden,
            response: forbidden(),
        })
        .unwrap_err();
        assert!(is_terminal(&error), "{error:?}");
    }

    #[test]
    fn installation_webhooks_map_to_distinct_lifecycle_actions() {
        assert_eq!(
            installation_lifecycle_action(Some("created")),
            Some(InstallationLifecycleAction::SyncNow)
        );
        assert_eq!(
            installation_lifecycle_action(Some("unsuspend")),
            Some(InstallationLifecycleAction::Start)
        );
        assert_eq!(
            installation_lifecycle_action(Some("suspend")),
            Some(InstallationLifecycleAction::Pause)
        );
        assert_eq!(
            installation_lifecycle_action(Some("deleted")),
            Some(InstallationLifecycleAction::Purge)
        );
        assert_eq!(
            installation_lifecycle_action(Some("new_permissions_accepted")),
            None
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
        fn persist(&mut self, state: SchedulerState) {
            self.persisted = Some(state);
        }

        fn schedule_tick(&mut self, generation: u64) {
            self.scheduled.push(generation);
        }

        async fn sweep(&mut self) -> HandlerResult<usize> {
            self.swept = true;
            match self.sweep_failure.take() {
                Some(error) => Err(error),
                None => Ok(3),
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

        let outcome = run_scheduler_tick(
            &mut restate,
            SchedulerState::armed(42),
            SchedulerTick(Some(42)),
        )
        .await
        .unwrap();

        assert_eq!(outcome, SchedulerTickOutcome::Swept { repositories: 3 });
        assert!(restate.swept);
        assert_eq!(restate.scheduled, vec![42]);
        assert_eq!(restate.persisted, Some(SchedulerState::armed(42)));
    }

    #[tokio::test]
    async fn a_stale_tick_neither_sweeps_nor_schedules() {
        let mut restate = RecordedRestate::default();

        let outcome = run_scheduler_tick(
            &mut restate,
            SchedulerState::armed(43),
            SchedulerTick(Some(42)),
        )
        .await
        .unwrap();

        assert_eq!(outcome, SchedulerTickOutcome::Dropped);
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

        let synced = run_repo_reconcile(&mut restate).await.unwrap();

        assert_eq!(synced, 2);
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

    /// An `io::Write` the test subscriber can hand out repeatedly.
    #[derive(Clone, Default)]
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LogSink {
        fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
            let sink = self.clone();
            tracing_subscriber::fmt()
                .with_writer(move || sink.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .finish()
        }

        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Everything the service logs while `run` executes, as the operator would see it.
    fn captured_logs(run: impl FnOnce()) -> String {
        let sink = LogSink::default();
        tracing::subscriber::with_default(sink.subscriber(), run);
        sink.contents()
    }

    #[tokio::test]
    async fn a_completed_handler_logs_its_key_outcome_and_duration() {
        let sink = LogSink::default();

        let outcome = traced("PullRequest/merge", "7#9", async {
            Ok(Json::from(ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            }))
        })
        .with_subscriber(sink.subscriber())
        .await
        .unwrap();

        assert!(matches!(
            outcome.into_inner(),
            ActionOutcome::Rejected { .. }
        ));
        let logs = sink.contents();
        assert!(logs.contains("INFO"), "{logs}");
        assert!(logs.contains(r#"handler="PullRequest/merge""#), "{logs}");
        assert!(logs.contains(r#"key="7#9""#), "{logs}");
        assert!(
            logs.contains(
                r#"outcome="rejected: GitHub reports this pull request is not mergeable""#
            ),
            "{logs}"
        );
        assert!(logs.contains("elapsed_ms="), "{logs}");
    }

    #[tokio::test]
    async fn a_failed_handler_logs_the_cause_at_warn() {
        let sink = LogSink::default();

        let result = traced::<()>("WebhookIngress/dispatch", "pull_request.opened", async {
            Err(TerminalError::new("pull_request webhook is missing number").into())
        })
        .with_subscriber(sink.subscriber())
        .await;

        assert!(result.is_err());
        let logs = sink.contents();
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains(r#"key="pull_request.opened""#), "{logs}");
        assert!(logs.contains(r#"outcome="failed""#), "{logs}");
        assert!(
            logs.contains("Terminal error [500]: pull_request webhook is missing number"),
            "{logs}"
        );
    }

    #[tokio::test]
    async fn polled_reads_complete_quietly_at_debug() {
        let sink = LogSink::default();

        traced_read("PullRequest/status", "7#9", async {
            Ok(Json::from(Option::<PrState>::None))
        })
        .with_subscriber(sink.subscriber())
        .await
        .unwrap();

        let logs = sink.contents();
        assert!(logs.contains("DEBUG"), "{logs}");
        assert!(logs.contains(r#"outcome="absent""#), "{logs}");
    }

    #[test]
    fn an_unparsable_seconds_setting_falls_back_and_warns_naming_the_variable() {
        let mut resolved = None;
        let logs = captured_logs(|| {
            resolved = Some(resolve_seconds("SYNC_DEBOUNCE_SECONDS", Some("20s"), 20));
        });

        assert_eq!(resolved, Some(20));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("SYNC_DEBOUNCE_SECONDS"), "{logs}");
        assert!(logs.contains("20s"), "{logs}");
    }

    #[test]
    fn configured_and_unset_seconds_settings_resolve_silently() {
        let mut resolved = Vec::new();
        let logs = captured_logs(|| {
            resolved.push(resolve_seconds(
                "RECONCILE_INTERVAL_SECONDS",
                Some("900"),
                3600,
            ));
            resolved.push(resolve_seconds("RECONCILE_INTERVAL_SECONDS", None, 3600));
            resolved.push(resolve_seconds(
                "RECONCILE_INTERVAL_SECONDS",
                Some(""),
                3600,
            ));
        });

        assert_eq!(resolved, vec![900, 3600, 3600]);
        assert!(logs.is_empty(), "{logs}");
    }

    /// A delivery as the web edge forwards it, for installation 1 and repository 7.
    fn delivery(event: &str, action: Option<&str>) -> WebhookEvent {
        WebhookEvent {
            event: event.to_owned(),
            action: action.map(str::to_owned),
            installation_id: Some(1),
            repository_id: Some(7),
            owner: Some("acme".to_owned()),
            repo: Some("api".to_owned()),
            number: Some(9),
            sha: Some("abc123".to_owned()),
            pull_requests: Vec::new(),
            sync_completion_id: None,
        }
    }

    #[test]
    fn deliveries_for_another_installation_are_rejected_naming_both_ids() {
        let mut event = delivery("pull_request", Some("opened"));
        event.installation_id = Some(5);

        let rejection = route_webhook(1, &event).unwrap_err();

        assert_eq!(
            rejection.to_string(),
            "webhook installation 5 does not match configured installation 1"
        );
    }

    #[test]
    fn deliveries_without_routing_fields_are_rejected_naming_the_missing_ones() {
        let mut pull = delivery("pull_request", Some("opened"));
        pull.repository_id = None;
        pull.number = None;
        assert_eq!(
            route_webhook(1, &pull).unwrap_err().to_string(),
            "pull_request webhook is missing repository_id, number"
        );

        let mut check = delivery("check_run", Some("completed"));
        check.sha = None;
        assert_eq!(
            route_webhook(1, &check).unwrap_err().to_string(),
            "commit webhook is missing sha"
        );

        let mut installation = delivery("installation", Some("suspend"));
        installation.installation_id = None;
        assert_eq!(
            route_webhook(1, &installation).unwrap_err().to_string(),
            "installation webhook is missing installation_id"
        );
    }

    #[test]
    fn a_dashboard_sync_routes_to_the_pull_request_bypassing_the_debounce() {
        let mut event = delivery("pull_request", Some(DASHBOARD_SYNC_ACTION));
        event.sync_completion_id = Some("completion-1".to_owned());

        assert_eq!(
            route_webhook(1, &event).unwrap(),
            WebhookRoute::SyncPullRequest(SyncRequest {
                repository_id: 7,
                owner: "acme".to_owned(),
                repo: "api".to_owned(),
                number: 9,
                bypass_debounce: true,
                completion_id: Some("completion-1".to_owned()),
            })
        );
        assert!(matches!(
            route_webhook(1, &delivery("pull_request", Some("synchronize"))).unwrap(),
            WebhookRoute::SyncPullRequest(request) if !request.bypass_debounce
        ));
        assert_eq!(
            route_webhook(1, &delivery("pull_request", Some("closed"))).unwrap(),
            WebhookRoute::ClosePullRequest(PrKey::new(7, 9))
        );
    }

    #[test]
    fn commit_deliveries_route_to_the_repository_with_the_pull_requests_github_named() {
        let mut event = delivery("check_run", Some("completed"));
        event.pull_requests = vec![9, 12];

        assert_eq!(
            route_webhook(1, &event).unwrap(),
            WebhookRoute::SyncSha(SyncShaRequest {
                repository_id: 7,
                owner: "acme".to_owned(),
                repo: "api".to_owned(),
                sha: "abc123".to_owned(),
                pull_requests: vec![9, 12],
            })
        );
    }

    #[test]
    fn installation_deliveries_route_to_the_installation_object() {
        assert_eq!(
            route_webhook(1, &delivery("installation", Some("suspend"))).unwrap(),
            WebhookRoute::Installation {
                installation_id: 1,
                action: InstallationLifecycleAction::Pause,
            }
        );
        assert_eq!(
            route_webhook(1, &delivery("installation_repositories", Some("added"))).unwrap(),
            WebhookRoute::Installation {
                installation_id: 1,
                action: InstallationLifecycleAction::SyncNow,
            }
        );
    }

    #[test]
    fn routed_kinds_with_unhandled_actions_are_ignored_not_rejected() {
        for (event, action) in [
            ("pull_request", "assigned"),
            ("check_suite", "requested"),
            ("check_run", "rerequested"),
            ("installation", "new_permissions_accepted"),
        ] {
            assert_eq!(
                route_webhook(1, &delivery(event, Some(action))).unwrap(),
                WebhookRoute::Ignore,
                "{event}.{action}"
            );
        }
    }
}
