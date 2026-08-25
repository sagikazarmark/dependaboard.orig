use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use dependaboard_core::{
    ActionLog, ActionOutcome, BatchProgress, BulkActionKind, BulkRequest, Classification,
    CommandRequest, DependabotCommand, GithubErrorResponse, MAX_BATCH_TARGETS, MergeRequest,
    Operation, PrKey, PrState, RejectReason, RepoRecord, SyncRequest, SyncShaRequest,
    TargetProgressState, UpdateBranchRequest, WebhookEvent, classify_github_error, valid_batch_id,
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

#[restate_sdk::object(ingress_private)]
impl PullRequest {
    #[handler]
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
        if state
            .last_synced_at
            .is_some_and(|last| now.saturating_sub(last) < self.debounce.as_secs())
        {
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

    #[handler]
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

    #[handler]
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

    #[handler]
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

    #[handler]
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
                }))
                .send();
        }
        Ok(Json::from(outcome))
    }

    #[handler(ingress_private = false)]
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
        let started = ctx.get::<bool>(SCHEDULER_STARTED).await?.unwrap_or(false);
        let tick_pending = ctx
            .get::<bool>(SCHEDULER_TICK_PENDING)
            .await?
            .unwrap_or(false);
        if started && tick_pending {
            return Ok(());
        }
        let current_generation = ctx.get::<u64>(SCHEDULER_GENERATION).await?.unwrap_or(0);
        let generation = if started && current_generation > 0 {
            current_generation
        } else {
            next_scheduler_generation(current_generation)?
        };
        ctx.set(SCHEDULER_GENERATION, generation);
        ctx.set(SCHEDULER_STARTED, true);
        ctx.set(SCHEDULER_TICK_PENDING, true);
        ctx.object_client::<InstallationSyncClient>(ctx.key())
            .tick(SchedulerTick(Some(generation)))
            .send();
        Ok(())
    }

    #[handler]
    async fn tick(&self, ctx: ObjectContext<'_>, generation: SchedulerTick) -> HandlerResult<()> {
        let started = ctx.get::<bool>(SCHEDULER_STARTED).await?.unwrap_or(false);
        let tick_pending = ctx
            .get::<bool>(SCHEDULER_TICK_PENDING)
            .await?
            .unwrap_or(false);
        let mut current_generation = ctx.get::<u64>(SCHEDULER_GENERATION).await?.unwrap_or(0);
        if !started {
            return Ok(());
        }
        let generation = match generation.0 {
            Some(generation) => generation,
            None if tick_pending => return Ok(()),
            None => {
                current_generation = next_scheduler_generation(current_generation)?;
                ctx.set(SCHEDULER_GENERATION, current_generation);
                current_generation
            }
        };
        if !scheduler_tick_is_current(started, current_generation, generation) {
            return Ok(());
        }
        ctx.clear(SCHEDULER_TICK_PENDING);
        perform_installation_sync(&ctx, self.github.clone(), self.store.clone()).await?;
        ctx.set(SCHEDULER_TICK_PENDING, true);
        ctx.object_client::<InstallationSyncClient>(ctx.key())
            .tick(SchedulerTick(Some(generation)))
            .send_after(self.interval);
        Ok(())
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

fn scheduler_tick_is_current(started: bool, current: u64, incoming: u64) -> bool {
    started && current == incoming
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
        let github = self.github.clone();
        let owner = repository.owner.clone();
        let repo = repository.repo.clone();
        let repository_id = repository.repository_id;
        let pulls = ctx
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
        let pulls = external_read(pulls)?;
        for request in &pulls {
            ctx.object_client::<PullRequestClient>(request_key(request).to_string())
                .sync(Json::from(request.clone()))
                .call()
                .await?;
        }
        let live = pulls
            .iter()
            .map(|request| request.number)
            .collect::<Vec<_>>();
        let store = self.store.clone();
        ctx.run(move || async move {
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
        Some("opened" | "reopened" | "synchronize" | "edited" | "labeled" | "unlabeled") => {
            ctx.object_client::<PullRequestClient>(key)
                .sync(Json::from(SyncRequest {
                    repository_id,
                    owner: owner.clone(),
                    repo: repo.clone(),
                    number,
                    observed_sha: event.sha.clone(),
                }))
                .send();
        }
        _ => {}
    }
    Ok(())
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

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
        .unwrap_or_else(|_| "0.0.0.0:9080".to_owned())
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
        .or_else(|_| env::var("RESTATE_API_KEY"))
        .ok();
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
        let mut request = client.post(&url).json(&());
        if let Some(api_key) = &api_key {
            request = request.bearer_auth(api_key);
        }
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

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use dependaboard_core::{CheckStatus, PrRecord, PrTarget, UpdateType, UserId};

    use super::*;

    fn target() -> PrTarget {
        PrTarget {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            expected_sha: "abc123".to_owned(),
            title: "Bump serde".to_owned(),
        }
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
            mergeable: None,
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
    fn scheduler_generations_are_monotonic_and_cannot_wrap() {
        assert_eq!(next_scheduler_generation(41).unwrap(), 42);
        assert!(next_scheduler_generation(u64::MAX).is_err());
        assert!(scheduler_tick_is_current(true, 42, 42));
        assert!(!scheduler_tick_is_current(false, 42, 42));
        assert!(!scheduler_tick_is_current(true, 43, 42));
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
