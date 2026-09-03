//! The `InstallationSync` virtual object: one per GitHub App installation, running the
//! perpetual reconcile chain and fanning each sweep out to every repository.

use std::time::Duration;

use bytes::Bytes;
use dependaboard_core::{Operation, unix_seconds};
use dependaboard_github::{GithubApi, GithubClient};
use dependaboard_store::{LibSqlPrStore, PrStore};
use restate_sdk::prelude::*;

use crate::{
    github::{RestateGithubStep, read_result, run_github_step},
    handler::{HandlerOutcome, traced},
    repo_sync::RepoSyncClient,
    store::{store_failure, store_retry_policy},
};

const SCHEDULER_STARTED: &str = "scheduler_started";
const SCHEDULER_GENERATION: &str = "scheduler_generation";
const SCHEDULER_TICK_PENDING: &str = "scheduler_tick_pending";

#[derive(Clone)]
pub(crate) struct InstallationSync {
    pub(crate) github: GithubClient,
    pub(crate) store: LibSqlPrStore,
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

#[cfg(test)]
mod tests {
    use super::*;

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
