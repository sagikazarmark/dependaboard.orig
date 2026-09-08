//! The `InstallationSync` virtual object: one per GitHub App installation, running the
//! perpetual reconcile chain and fanning each sweep out to every repository.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use dependaboard_core::{Operation, RepoRecord, unix_seconds};
use dependaboard_store::PrStore;
use restate_sdk::prelude::*;

use crate::{
    github::{GithubApiHandle, RestateGithubStep, read_result, run_github_step},
    handler::{HandlerOutcome, traced},
    repo_sync::RepoSyncClient,
    retirement::{RestateRetirements, RetirementEffects, retire_pending},
    store::{StoreStepContext, store_failure, store_retry_policy},
};

const SCHEDULER_STARTED: &str = "scheduler_started";
const SCHEDULER_GENERATION: &str = "scheduler_generation";
const SCHEDULER_TICK_PENDING: &str = "scheduler_tick_pending";

#[derive(Clone)]
pub(crate) struct InstallationSync {
    pub(crate) github: GithubApiHandle,
    pub(crate) store: Arc<dyn PrStore>,
    pub(crate) interval: Duration,
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
            let repositories = perform_installation_sync(&ctx, &self.github, &self.store).await?;
            Ok(format!("fanned out to {repositories} repositories"))
        })
        .await
        .map(|_| ())
    }

    #[handler]
    async fn pause(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        traced("InstallationSync/pause", ctx.key(), async {
            let state = read_scheduler_state(&ctx).await?;
            write_scheduler_state(&ctx, scheduler_pause_transition(state)?);
            Ok(())
        })
        .await
    }

    #[handler]
    async fn purge(&self, ctx: ObjectContext<'_>) -> HandlerResult<()> {
        traced("InstallationSync/purge", ctx.key(), async {
            let state = read_scheduler_state(&ctx).await?;
            write_scheduler_state(&ctx, scheduler_pause_transition(state)?);
            let installation_id = ctx
                .key()
                .parse::<u64>()
                .map_err(|_| TerminalError::new("installation key must be an integer"))?;
            let mut restate = RestatePurgeEffects {
                ctx: &ctx,
                store: &self.store,
                installation_id,
            };
            let mut retirements = RestateRetirements {
                ctx: &ctx,
                store: &self.store,
            };
            let pull_requests = run_installation_purge(&mut restate, &mut retirements).await?;
            Ok(format!("retired {pull_requests} pull requests"))
        })
        .await
        .map(|_| ())
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

/// Stops the chain: `pause` and `purge` both end here.
///
/// Bumping the generation is what actually stops it. The tick the chain has queued or
/// delayed inside Restate cannot be recalled, so it arrives later carrying the old
/// generation, and `scheduler_tick_transition` drops it. Clearing `started` keeps
/// `start` honest about re-arming, and clearing `tick_pending` lets it.
fn scheduler_pause_transition(state: SchedulerState) -> HandlerResult<SchedulerState> {
    Ok(SchedulerState {
        started: false,
        tick_pending: false,
        generation: next_scheduler_generation(state.generation)?,
    })
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
    github: &'a GithubApiHandle,
    store: &'a Arc<dyn PrStore>,
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
        perform_installation_sync(self.ctx, self.github, self.store).await
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

/// Side effects an installation sweep asks of Restate, GitHub and the store, abstracted
/// so `run_installation_sync` can be exercised against a recording fake without a runtime.
trait InstallationSyncEffects {
    /// Enumerates every repository the installation grants access to: complete, or an
    /// error. A partial listing must never become the authoritative set.
    fn list_repositories(&mut self) -> impl Future<Output = HandlerResult<Vec<RepoRecord>>> + Send;
    /// Makes `repositories` the installation's set in the projection, upserting them and
    /// dropping the rest. The pull requests that went with the dropped ones are queued
    /// for retirement by the store, fenced by the sweep's start; the drain that follows
    /// tells them, so nothing here depends on what this step returns.
    fn replace_repositories(
        &mut self,
        repositories: &[RepoRecord],
    ) -> impl Future<Output = HandlerResult<()>> + Send;
    /// Fans a reconcile out to the repository's `RepoSync` object.
    fn reconcile_repository(&mut self, repository: RepoRecord);
}

struct RestateSyncEffects<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    github: &'a GithubApiHandle,
    store: &'a Arc<dyn PrStore>,
    reconcile_start: u64,
}

impl InstallationSyncEffects for RestateSyncEffects<'_, '_> {
    async fn list_repositories(&mut self) -> HandlerResult<Vec<RepoRecord>> {
        let github = self.github.clone();
        let repositories = run_github_step(&mut RestateGithubStep {
            ctx: self.ctx,
            name: "list-installation-repositories",
            operation: Operation::Read,
            known_resource: false,
            call: move || {
                let github = github.clone();
                async move { github.list_installation_repositories().await }
            },
        })
        .await?;
        read_result(repositories)
    }

    async fn replace_repositories(&mut self, repositories: &[RepoRecord]) -> HandlerResult<()> {
        let store = self.store.clone();
        let installation_id = self.github.installation_id();
        let reconcile_start = self.reconcile_start;
        let repositories = repositories.to_vec();
        self.ctx
            .run_store_step(
                "replace-installation-repositories",
                store_retry_policy(),
                move || async move {
                    store
                        .replace_installation_repos(installation_id, &repositories, reconcile_start)
                        .await
                        .map(drop)
                        .map_err(store_failure)
                },
            )
            .await?;
        Ok(())
    }

    fn reconcile_repository(&mut self, repository: RepoRecord) {
        self.ctx
            .object_client::<RepoSyncClient>(repository.repository_id.to_string())
            .reconcile(Json::from(repository))
            .send();
    }
}

/// Re-enumerates the installation's repositories, makes the projection match, retires the
/// durable state of every pull request the outbox holds — those that left with a
/// repository just now, and any an earlier prune removed without getting to tell — then
/// fans a reconcile out to each repository that remains. Resolves to how many were fanned
/// out to.
async fn run_installation_sync<E: InstallationSyncEffects, R: RetirementEffects>(
    restate: &mut E,
    retirements: &mut R,
) -> HandlerResult<usize> {
    let repositories = restate.list_repositories().await?;
    restate.replace_repositories(&repositories).await?;
    retire_pending(retirements).await?;
    let count = repositories.len();
    for repository in repositories {
        restate.reconcile_repository(repository);
    }
    Ok(count)
}

/// One installation sweep bound to Restate; resolves to how many repositories it fanned
/// out to.
async fn perform_installation_sync(
    ctx: &ObjectContext<'_>,
    github: &GithubApiHandle,
    store: &Arc<dyn PrStore>,
) -> HandlerResult<usize> {
    let reconcile_start = ctx
        .run(|| async { Ok(unix_seconds()) })
        .name("installation-reconcile-clock")
        .await?;
    let mut restate = RestateSyncEffects {
        ctx,
        github,
        store,
        reconcile_start,
    };
    let mut retirements = RestateRetirements { ctx, store };
    run_installation_sync(&mut restate, &mut retirements).await
}

/// Side effects a purge asks of Restate and the store, abstracted so
/// `run_installation_purge` can be exercised against a recording fake without a runtime.
trait InstallationPurgeEffects {
    /// Drops the installation's repositories and pull requests from the projection. The
    /// pull requests are queued for retirement by the store, unfenced: the App has lost
    /// the installation, so no webhook can reopen them.
    fn purge_projection(&mut self) -> impl Future<Output = HandlerResult<()>> + Send;
}

struct RestatePurgeEffects<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    store: &'a Arc<dyn PrStore>,
    installation_id: u64,
}

impl InstallationPurgeEffects for RestatePurgeEffects<'_, '_> {
    async fn purge_projection(&mut self) -> HandlerResult<()> {
        let store = self.store.clone();
        let installation_id = self.installation_id;
        self.ctx
            .run_store_step(
                "purge-installation",
                store_retry_policy(),
                move || async move {
                    store
                        .purge_installation(installation_id)
                        .await
                        .map(drop)
                        .map_err(store_failure)
                },
            )
            .await?;
        Ok(())
    }
}

/// Purges the installation's projection, then retires the durable state of every pull
/// request the outbox holds, so no object keeps serving a snapshot for a repository the
/// App can no longer see. Resolves to how many pull requests were retired.
async fn run_installation_purge<E: InstallationPurgeEffects, R: RetirementEffects>(
    restate: &mut E,
    retirements: &mut R,
) -> HandlerResult<usize> {
    restate.purge_projection().await?;
    retire_pending(retirements).await
}

#[cfg(test)]
mod tests {
    use dependaboard_core::PrKey;

    use super::*;
    use crate::{pull_request::ClosedRequest, test_support::RecordedRetirements};

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

    #[test]
    fn pausing_kills_the_tick_in_flight_and_lets_start_arm_a_fresh_chain() {
        let paused = scheduler_pause_transition(SchedulerState::armed(42)).unwrap();

        assert_eq!(
            paused,
            SchedulerState {
                started: false,
                tick_pending: false,
                generation: 43,
            }
        );
        assert_eq!(
            scheduler_tick_transition(paused, SchedulerTick(Some(42))).unwrap(),
            None,
            "the tick the chain had in flight carries the old generation and dies on arrival"
        );
        assert_eq!(
            scheduler_start_transition(paused).unwrap(),
            Some(SchedulerState::armed(44))
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

    /// Stands in for Restate and the store during an installation purge and records what
    /// the purge asked of them.
    #[derive(Default)]
    struct RecordedPurge {
        purge_failure: Option<HandlerError>,
        purged: bool,
    }

    impl InstallationPurgeEffects for RecordedPurge {
        async fn purge_projection(&mut self) -> HandlerResult<()> {
            self.purged = true;
            match self.purge_failure.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    #[tokio::test]
    async fn a_purge_retires_the_durable_state_of_every_pull_request_it_removed() {
        let mut restate = RecordedPurge::default();
        // What the projection queued when the installation's repositories went: unfenced,
        // since the App has lost the installation and nothing can reopen them.
        let mut retirements = RecordedRetirements::queued(
            &[PrKey::new(7, 3), PrKey::new(7, 9), PrKey::new(8, 1)],
            None,
        );

        let retired = run_installation_purge(&mut restate, &mut retirements)
            .await
            .unwrap();

        assert_eq!(retired, 3);
        assert!(restate.purged);
        assert_eq!(
            retirements.closed,
            vec![
                (PrKey::new(7, 3), ClosedRequest::default()),
                (PrKey::new(7, 9), ClosedRequest::default()),
                (PrKey::new(8, 1), ClosedRequest::default()),
            ],
            "no pull request of a deleted installation may keep a snapshot"
        );
        assert_eq!(retirements.acknowledged, vec![3]);
    }

    #[tokio::test]
    async fn a_failed_purge_retires_nothing() {
        let mut restate = RecordedPurge {
            purge_failure: Some(TerminalError::new("projection store is read-only").into()),
            ..Default::default()
        };
        let mut retirements = RecordedRetirements::queued(&[PrKey::new(7, 3)], None);

        let outcome = run_installation_purge(&mut restate, &mut retirements).await;

        assert!(
            outcome.is_err(),
            "the failed purge stays visible to Restate"
        );
        assert!(
            retirements.closed.is_empty(),
            "the projection still holds the rows, so their objects keep their state"
        );
    }

    fn installed_repository(repository_id: u64) -> RepoRecord {
        RepoRecord {
            repository_id,
            installation_id: 1,
            owner: "acme".to_owned(),
            repo: format!("repo-{repository_id}"),
            merge_method: None,
            synced_at: 0,
        }
    }

    /// Stands in for Restate, GitHub and the store during an installation sweep and
    /// records what the sweep asked of them.
    #[derive(Default)]
    struct RecordedSweep {
        repositories: Vec<RepoRecord>,
        listing_failure: Option<HandlerError>,
        replace_failure: Option<HandlerError>,
        replaced: Option<Vec<u64>>,
        reconciled: Vec<u64>,
    }

    impl InstallationSyncEffects for RecordedSweep {
        async fn list_repositories(&mut self) -> HandlerResult<Vec<RepoRecord>> {
            match self.listing_failure.take() {
                Some(error) => Err(error),
                None => Ok(self.repositories.clone()),
            }
        }

        async fn replace_repositories(&mut self, repositories: &[RepoRecord]) -> HandlerResult<()> {
            self.replaced = Some(
                repositories
                    .iter()
                    .map(|repository| repository.repository_id)
                    .collect(),
            );
            match self.replace_failure.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn reconcile_repository(&mut self, repository: RepoRecord) {
            self.reconciled.push(repository.repository_id);
        }
    }

    #[tokio::test]
    async fn a_sweep_retires_every_pull_request_that_left_with_its_repository_under_the_sweeps_fence()
     {
        let mut restate = RecordedSweep {
            repositories: vec![installed_repository(7), installed_repository(9)],
            ..Default::default()
        };
        // What the projection queued when repositories 8 and 12 were dropped, whether by
        // this run of the replace or by an earlier attempt whose result never reached the
        // journal: the sweep cannot tell, and need not.
        let mut retirements = RecordedRetirements::queued(
            &[PrKey::new(8, 1), PrKey::new(8, 4), PrKey::new(12, 2)],
            Some(1_000),
        );

        let swept = run_installation_sync(&mut restate, &mut retirements)
            .await
            .unwrap();

        assert_eq!(swept, 2);
        assert_eq!(
            restate.replaced,
            Some(vec![7, 9]),
            "the listing is the installation's authoritative repository set"
        );
        assert_eq!(
            retirements.closed,
            vec![
                (
                    PrKey::new(8, 1),
                    ClosedRequest {
                        synced_before: Some(1_000)
                    }
                ),
                (
                    PrKey::new(8, 4),
                    ClosedRequest {
                        synced_before: Some(1_000)
                    }
                ),
                (
                    PrKey::new(12, 2),
                    ClosedRequest {
                        synced_before: Some(1_000)
                    }
                ),
            ],
            "no pull request of a repository that left may keep a snapshot, but one synced \
             since the sweep began was re-added behind its back and is spared"
        );
        assert_eq!(
            retirements.acknowledged,
            vec![3],
            "the outbox is emptied once every object has been told"
        );
        assert_eq!(
            restate.reconciled,
            vec![7, 9],
            "the repositories that remain are swept as before"
        );
    }

    #[tokio::test]
    async fn a_failed_replace_retires_nothing_and_reconciles_nothing() {
        let mut restate = RecordedSweep {
            repositories: vec![installed_repository(7)],
            replace_failure: Some(TerminalError::new("projection store is read-only").into()),
            ..Default::default()
        };
        let mut retirements = RecordedRetirements::queued(&[PrKey::new(8, 1)], Some(1_000));

        let outcome = run_installation_sync(&mut restate, &mut retirements).await;

        assert!(
            outcome.is_err(),
            "the failed replace stays visible to Restate"
        );
        assert!(
            retirements.closed.is_empty(),
            "the projection still holds the rows, so their objects keep their state"
        );
        assert!(
            restate.reconciled.is_empty(),
            "repositories are reconciled only once their rows are in place"
        );
    }

    #[tokio::test]
    async fn a_failed_listing_never_reaches_the_projection() {
        let mut restate = RecordedSweep {
            repositories: vec![installed_repository(7)],
            listing_failure: Some(
                TerminalError::new("GitHub read failed with HTTP 401: Bad credentials").into(),
            ),
            ..Default::default()
        };
        let mut retirements = RecordedRetirements::queued(&[PrKey::new(8, 1)], Some(1_000));

        let outcome = run_installation_sync(&mut restate, &mut retirements).await;

        assert!(outcome.is_err());
        assert_eq!(
            restate.replaced, None,
            "an unknown live set must not delete anything"
        );
        assert!(retirements.closed.is_empty());
        assert!(restate.reconciled.is_empty());
    }
}
