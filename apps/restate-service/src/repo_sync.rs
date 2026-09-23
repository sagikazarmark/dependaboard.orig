//! The `RepoSync` virtual object: one per repository, sweeping its open Dependabot pull
//! requests and fanning commit-status changes out to the pull requests they touch.

use std::{collections::BTreeSet, sync::Arc};

use dependaboard_core::{PrKey, PrRecord, RepoRecord, SyncRequest, SyncShaRequest};
use dependaboard_github::Operation;
use dependaboard_store::ProjectionWriter;
use restate_sdk::prelude::*;
use tracing::warn;

use crate::{
    clock::ClockStepContext,
    github::{GithubApiHandle, RestateGithubStep, read_result, run_github_step},
    handler::traced,
    pull_request::{PullRequestClient, request_key, short_sha},
    retirement::{Pruned, RestateRetirements, RetirementEffects, retire_pending},
    store::{StoreStepContext, StoreStepKind},
};

/// Side effects the repository's handlers ask of Restate, GitHub and the store — the
/// sweep's listing, calls and retention, and the commit-status fan-out's lookup and
/// sends — abstracted so `run_repo_reconcile` and `run_sync_sha` can be exercised
/// against a recording fake without a runtime.
trait RepoReconcileEffects {
    fn repository_id(&self) -> u64;
    /// The wall clock, in Unix seconds, journaled under `step` so a replay reads the
    /// same moment.
    fn now(&mut self, step: &'static str) -> impl Future<Output = HandlerResult<u64>> + Send;
    fn list_pull_requests(
        &mut self,
    ) -> impl Future<Output = HandlerResult<Vec<SyncRequest>>> + Send;
    /// Calls the pull request object `key` names to sync `request`, and waits on it. A
    /// Restate-to-Restate call only fails once the callee has failed terminally; retryable
    /// failures are retried inside `PullRequest::sync` and never surface here. The key is
    /// passed rather than derived here so a test sees where the call went.
    fn sync_pull_request(
        &mut self,
        key: &PrKey,
        request: &SyncRequest,
    ) -> impl Future<Output = Result<(), TerminalError>> + Send;
    /// Prunes the projection down to `live`, sparing rows synced since `synced_before`.
    /// The keys it removes are queued for retirement by the store under the same fence,
    /// so what this resolves to is not them but a [`Pruned`]: nothing to read, and no
    /// way to reach the end of a reconcile without handing it to the drain.
    fn retain_pull_requests(
        &mut self,
        live: &[u64],
        synced_before: u64,
    ) -> impl Future<Output = HandlerResult<Pruned>> + Send;
    /// The repository's pull requests whose head is `sha`, as the projection has them.
    fn pull_requests_at(
        &mut self,
        sha: &str,
    ) -> impl Future<Output = HandlerResult<Vec<PrRecord>>> + Send;
    /// Sends `request` one-way to the pull request object `key` names, to sync as soon as
    /// it is free. A send cannot fail; how the sync fares is the callee's to tell. The
    /// key is passed rather than derived here so a test sees where the send went.
    fn send_sync(&mut self, key: &PrKey, request: SyncRequest);
}

struct RestateReconcileEffects<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    github: &'a GithubApiHandle,
    store: &'a Arc<dyn ProjectionWriter>,
    repository_id: u64,
    owner: String,
    repo: String,
}

impl RepoReconcileEffects for RestateReconcileEffects<'_, '_> {
    fn repository_id(&self) -> u64 {
        self.repository_id
    }

    async fn now(&mut self, step: &'static str) -> HandlerResult<u64> {
        Ok(self.ctx.run_clock_step(step).await?)
    }

    async fn list_pull_requests(&mut self) -> HandlerResult<Vec<SyncRequest>> {
        let github = self.github.clone();
        let owner = self.owner.clone();
        let repo = self.repo.clone();
        let repository_id = self.repository_id;
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

    async fn sync_pull_request(
        &mut self,
        key: &PrKey,
        request: &SyncRequest,
    ) -> Result<(), TerminalError> {
        self.ctx
            .object_client::<PullRequestClient>(key.to_string())
            .sync(Json::from(request.clone()))
            .call()
            .await
    }

    async fn retain_pull_requests(
        &mut self,
        live: &[u64],
        synced_before: u64,
    ) -> HandlerResult<Pruned> {
        let store = self.store.clone();
        let repository_id = self.repository_id;
        let live = live.to_vec();
        self.ctx
            .run_store_step(
                "retain-live-pull-requests",
                StoreStepKind::Ordinary,
                move || async move {
                    store
                        .retain_prs(repository_id, &live, synced_before)
                        .await
                        .map(drop)
                },
            )
            .await?;
        Ok(Pruned::landed())
    }

    async fn pull_requests_at(&mut self, sha: &str) -> HandlerResult<Vec<PrRecord>> {
        let store = self.store.clone();
        let repository_id = self.repository_id;
        let sha = sha.to_owned();
        let matches = self
            .ctx
            .run_store_step(
                "resolve-prs-for-sha",
                StoreStepKind::Ordinary,
                move || async move { store.prs_for_sha(repository_id, &sha).await.map(Json::from) },
            )
            .await?;
        Ok(matches.into_inner())
    }

    fn send_sync(&mut self, key: &PrKey, request: SyncRequest) {
        self.ctx
            .object_client::<PullRequestClient>(key.to_string())
            .sync(Json::from(request))
            .send();
    }
}

/// Reads the fence, sweeps every listed pull request, prunes the projection down to the
/// listing under that fence, then retires the durable state of every pull request the
/// outbox holds — those pruning removed just now, and any an earlier prune removed
/// without getting to tell.
///
/// A pull request that fails terminally is logged and remembered rather than propagated,
/// so one unsyncable pull request can neither starve the rest of the repository nor skip
/// stale-row cleanup. Retention still requires a complete listing: if listing fails, the
/// live set is unknown and nothing is deleted. Resolves to how many pull requests synced.
async fn run_repo_reconcile<E: RepoReconcileEffects, R: RetirementEffects>(
    restate: &mut E,
    retirements: &mut R,
) -> HandlerResult<usize> {
    // The reconcile's first act, before the listing: a pull request written while the
    // listing is in flight is newer than the fence, so the prune the retain runs spares
    // it.
    let reconcile_start = restate.now("repo-reconcile-clock").await?;
    let pulls = restate.list_pull_requests().await?;
    let mut failed = Vec::new();
    for request in &pulls {
        // Keyed by the request, as every sender keys a sync, so it is that pull request's.
        let key = request_key(request);
        if let Err(error) = restate.sync_pull_request(&key, request).await {
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
    let pruned = restate.retain_pull_requests(&live, reconcile_start).await?;
    retire_pending(pruned, retirements).await?;
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

/// Fans a commit's status change out to the pull requests at that commit: the ones
/// GitHub named in the delivery — none for a fork's, several for a shared head — and
/// the ones the projection has at that head, each sent one sync. No match means nothing
/// is sent: a commit no pull request is at is not this repository's to reconcile.
/// Resolves to how many pull requests were told.
async fn run_sync_sha<E: RepoReconcileEffects>(
    restate: &mut E,
    request: &SyncShaRequest,
) -> HandlerResult<usize> {
    let at_head = restate.pull_requests_at(&request.sha).await?;
    let mut numbers = request
        .pull_requests
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    numbers.extend(at_head.into_iter().map(|pull| pull.number));
    let told = numbers.len();
    for number in numbers {
        let sync = SyncRequest {
            repository_id: request.repository_id,
            owner: request.owner.clone(),
            repo: request.repo.clone(),
            number,
            bypass_debounce: false,
            completion_id: None,
        };
        // Keyed by the request, as every sender keys a sync, so it is the same object.
        restate.send_sync(&request_key(&sync), sync);
    }
    Ok(told)
}

#[derive(Clone)]
pub(crate) struct RepoSync {
    pub(crate) github: GithubApiHandle,
    pub(crate) store: Arc<dyn ProjectionWriter>,
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
            let mut restate = RestateReconcileEffects {
                ctx: &ctx,
                github: &self.github,
                store: &self.store,
                repository_id: repository.repository_id,
                owner: repository.owner,
                repo: repository.repo,
            };
            let mut retirements = RestateRetirements {
                ctx: &ctx,
                store: &self.store,
            };
            let pull_requests = run_repo_reconcile(&mut restate, &mut retirements).await?;
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
            let mut restate = RestateReconcileEffects {
                ctx: &ctx,
                github: &self.github,
                store: &self.store,
                repository_id: request.repository_id,
                owner: request.owner.clone(),
                repo: request.repo.clone(),
            };
            let told = run_sync_sha(&mut restate, &request).await?;
            Ok(format!(
                "fanned out to {told} pull requests at {}",
                short_sha(&request.sha)
            ))
        })
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dependaboard_core::Retirement;

    use super::*;
    use crate::{
        pull_request::ClosedRequest,
        test_support::{CLOCK_EPOCH, FakeClock, RecordedRetirements, snapshot},
    };

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

    /// A row of the projection: pull request `number` of `acme/api`, at `abc123`.
    fn row_at_head(number: u64) -> PrRecord {
        PrRecord {
            id: PrKey::new(7, number).to_string(),
            number,
            ..snapshot()
        }
    }

    /// What the ingress routes a check delivery for `abc123` in `acme/api` to, with the
    /// pull requests GitHub named in it.
    fn commit_status(pull_requests: &[u64]) -> SyncShaRequest {
        SyncShaRequest {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            sha: "abc123".to_owned(),
            pull_requests: pull_requests.to_vec(),
        }
    }

    /// Stands in for Restate, GitHub and the store during a repository reconcile or a
    /// commit-status fan-out and records what each asked of them.
    #[derive(Default)]
    struct RecordedRepoSync {
        pulls: Vec<SyncRequest>,
        listing_failure: Option<HandlerError>,
        sync_failures: BTreeMap<u64, TerminalError>,
        retain_failure: Option<HandlerError>,
        /// The key of every pull request object the sweep called, in order.
        synced: Vec<PrKey>,
        retained: Option<Vec<u64>>,
        /// The fence the retain was asked to prune under.
        retained_under: Option<u64>,
        /// What the projection has at the head `pull_requests_at` is asked about.
        at_head: Vec<PrRecord>,
        /// The heads `pull_requests_at` was asked about.
        resolved: Vec<String>,
        /// Every sync sent one-way, in order: the key it went to and what it carried.
        sent: Vec<(PrKey, SyncRequest)>,
        /// The clock the handler reads, a second per reading, and the steps it read
        /// them under.
        clock: FakeClock,
        /// What the clock stood at when the listing was asked for: a fence read before
        /// it is strictly smaller.
        clock_when_listed: Option<u64>,
    }

    impl RepoReconcileEffects for RecordedRepoSync {
        fn repository_id(&self) -> u64 {
            7
        }

        async fn now(&mut self, step: &'static str) -> HandlerResult<u64> {
            Ok(self.clock.now(step))
        }

        async fn list_pull_requests(&mut self) -> HandlerResult<Vec<SyncRequest>> {
            self.clock_when_listed = Some(self.clock.peek());
            match self.listing_failure.take() {
                Some(error) => Err(error),
                None => Ok(self.pulls.clone()),
            }
        }

        async fn sync_pull_request(
            &mut self,
            key: &PrKey,
            request: &SyncRequest,
        ) -> Result<(), TerminalError> {
            self.synced.push(key.clone());
            match self.sync_failures.remove(&request.number) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        async fn retain_pull_requests(
            &mut self,
            live: &[u64],
            synced_before: u64,
        ) -> HandlerResult<Pruned> {
            self.retained = Some(live.to_vec());
            self.retained_under = Some(synced_before);
            match self.retain_failure.take() {
                Some(error) => Err(error),
                None => Ok(Pruned::landed()),
            }
        }

        async fn pull_requests_at(&mut self, sha: &str) -> HandlerResult<Vec<PrRecord>> {
            self.resolved.push(sha.to_owned());
            Ok(self.at_head.clone())
        }

        fn send_sync(&mut self, key: &PrKey, request: SyncRequest) {
            self.sent.push((key.clone(), request));
        }
    }

    /// A check delivery names a commit. GitHub names the pull requests it knows at that
    /// commit — none for a fork's, several for a shared head — and the projection may
    /// know others, so the fan-out is the union, each pull request told once.
    #[tokio::test]
    async fn a_commit_status_reaches_every_pull_request_at_that_head_once() {
        let mut restate = RecordedRepoSync {
            at_head: vec![row_at_head(12), row_at_head(19)],
            ..Default::default()
        };

        let told = run_sync_sha(&mut restate, &commit_status(&[19, 23]))
            .await
            .unwrap();

        assert_eq!(restate.resolved, vec!["abc123"]);
        assert_eq!(told, 3);
        assert_eq!(
            restate.sent,
            vec![
                (PrKey::new(7, 12), dependabot_pull(12)),
                (PrKey::new(7, 19), dependabot_pull(19)),
                (PrKey::new(7, 23), dependabot_pull(23)),
            ],
            "the ones GitHub named and the ones the projection has at that head, 19 once, \
             each sent to its own object as a plain sync that honours the debounce"
        );
    }

    #[tokio::test]
    async fn a_commit_no_pull_request_is_at_syncs_nothing() {
        let mut restate = RecordedRepoSync::default();

        let told = run_sync_sha(&mut restate, &commit_status(&[]))
            .await
            .unwrap();

        assert_eq!(told, 0);
        assert!(
            restate.sent.is_empty(),
            "a commit no pull request is at is not this repository's to sync"
        );
        assert!(
            restate.synced.is_empty(),
            "no match means ignore, not reconcile: no pull request is swept"
        );
        assert_eq!(
            restate.retained, None,
            "no match means ignore, not reconcile: nothing is pruned"
        );
    }

    #[tokio::test]
    async fn every_pruned_pull_request_has_its_durable_state_closed_under_the_sweeps_fence() {
        let mut restate = RecordedRepoSync {
            pulls: vec![dependabot_pull(12)],
            ..Default::default()
        };
        // What the projection queued when it pruned 3 and 9, whether by this run of the
        // retain or by an earlier attempt whose result never reached the journal.
        let mut retirements =
            RecordedRetirements::queued(&[PrKey::new(7, 3), PrKey::new(7, 9)], Some(CLOCK_EPOCH));

        run_repo_reconcile(&mut restate, &mut retirements)
            .await
            .unwrap();

        assert_eq!(restate.retained, Some(vec![12]));
        assert_eq!(
            restate.retained_under,
            Some(CLOCK_EPOCH),
            "the projection prunes under the sweep's start, the fence the retirements carry"
        );
        assert_eq!(
            retirements.closed,
            vec![
                (
                    PrKey::new(7, 3),
                    ClosedRequest {
                        synced_before: Some(CLOCK_EPOCH)
                    }
                ),
                (
                    PrKey::new(7, 9),
                    ClosedRequest {
                        synced_before: Some(CLOCK_EPOCH)
                    }
                ),
            ],
            "a row pruned from the projection must not keep a snapshot in its object, \
             unless it was re-synced since the sweep began"
        );
        assert_eq!(retirements.acknowledged, vec![2]);
    }

    /// The token a prune hands the drain unlocks it; it does not scope it. Every prune
    /// writes to one outbox and any drain empties the whole of it, which is what makes a
    /// drain lost to a crash harmless — the next handler to prune anything finishes it —
    /// and it is the property a prune that owned its queue entries would destroy.
    #[tokio::test]
    async fn one_repositorys_drain_also_retires_what_another_repositorys_sweep_pruned() {
        let mut restate = RecordedRepoSync {
            pulls: vec![dependabot_pull(12)],
            ..Default::default()
        };
        // Repository 8's sweep pruned 8#1 an hour ago under its own fence and died before
        // draining; repository 7's retain has just queued 7#3 under this sweep's.
        let mut retirements = RecordedRetirements {
            pending: vec![
                Retirement {
                    id: 4,
                    key: PrKey::new(8, 1),
                    synced_before: Some(CLOCK_EPOCH - 3_600),
                },
                Retirement {
                    id: 5,
                    key: PrKey::new(7, 3),
                    synced_before: Some(CLOCK_EPOCH),
                },
            ],
            ..Default::default()
        };

        let swept = run_repo_reconcile(&mut restate, &mut retirements)
            .await
            .unwrap();

        assert_eq!(swept, 1);
        assert_eq!(
            retirements.closed,
            vec![
                (
                    PrKey::new(8, 1),
                    ClosedRequest {
                        synced_before: Some(CLOCK_EPOCH - 3_600)
                    }
                ),
                (
                    PrKey::new(7, 3),
                    ClosedRequest {
                        synced_before: Some(CLOCK_EPOCH)
                    }
                ),
            ],
            "this repository's drain tells the pull request another repository's sweep \
             pruned, under the fence that sweep recorded and not this one's"
        );
        assert_eq!(
            retirements.acknowledged,
            vec![5],
            "the whole outbox is emptied, not this repository's share of it"
        );
    }

    #[tokio::test]
    async fn the_reconcile_prunes_under_the_instant_read_before_its_listing() {
        let mut restate = RecordedRepoSync {
            pulls: vec![dependabot_pull(12)],
            ..Default::default()
        };

        run_repo_reconcile(&mut restate, &mut RecordedRetirements::default())
            .await
            .unwrap();

        let fence = restate
            .retained_under
            .expect("the retain ran under a fence");
        let listed = restate
            .clock_when_listed
            .expect("the reconcile asked for the listing");
        assert!(
            fence < listed,
            "the fence is read before the listing is asked for, not after it returns: a \
             pull request opened while the listing was in flight and synced by its own \
             webhook is newer than a fence read first and is spared, where one read once \
             the listing returned would take it; {fence} is not earlier than {listed}"
        );
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
        let mut retirements = RecordedRetirements::queued(&[PrKey::new(7, 5)], Some(900));

        let outcome = run_repo_reconcile(&mut restate, &mut retirements).await;

        assert_eq!(
            restate.synced,
            vec![PrKey::new(7, 12), PrKey::new(7, 19), PrKey::new(7, 23)],
            "each listed pull request is swept through its own object"
        );
        assert_eq!(
            restate.retained,
            Some(vec![12, 19, 23]),
            "the failed pull request is still open on GitHub, so its row must survive"
        );
        assert_eq!(
            retirements.closed_keys(),
            vec![PrKey::new(7, 5)],
            "the pruned pull request's durable state is retired despite the failed sync"
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
        let mut retirements = RecordedRetirements::queued(&[PrKey::new(7, 3)], Some(900));

        let outcome = run_repo_reconcile(&mut restate, &mut retirements).await;

        assert!(outcome.is_err());
        assert!(restate.synced.is_empty());
        assert_eq!(
            restate.retained, None,
            "an unknown live set must not delete anything"
        );
        assert!(
            retirements.closed.is_empty(),
            "nothing was pruned, so no durable state may be retired"
        );
    }

    #[tokio::test]
    async fn a_failed_retain_retires_nothing() {
        let mut restate = RecordedRepoSync {
            pulls: vec![dependabot_pull(12)],
            retain_failure: Some(TerminalError::new("projection store is read-only").into()),
            ..Default::default()
        };
        let mut retirements = RecordedRetirements::queued(&[PrKey::new(7, 3)], Some(900));

        let outcome = run_repo_reconcile(&mut restate, &mut retirements).await;

        assert!(
            outcome.is_err(),
            "the failed retain stays visible to Restate"
        );
        assert!(
            retirements.closed.is_empty(),
            "the prune did not land, so the projection still holds the rows and their \
             objects keep their state: a drain follows a prune, and only a prune"
        );
        assert!(
            retirements.acknowledged.is_empty(),
            "nothing was told, so nothing is forgotten"
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

        let error = run_repo_reconcile(&mut restate, &mut RecordedRetirements::default())
            .await
            .unwrap_err();

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

        let synced = run_repo_reconcile(&mut restate, &mut RecordedRetirements::default())
            .await
            .unwrap();

        assert_eq!(synced, 2);
        assert_eq!(
            restate.synced,
            vec![PrKey::new(7, 12), PrKey::new(7, 19)],
            "each listed pull request is swept through its own object"
        );
        assert_eq!(restate.retained, Some(vec![12, 19]));
    }

    #[tokio::test]
    async fn an_empty_listing_still_prunes_the_projection() {
        let mut restate = RecordedRepoSync::default();

        run_repo_reconcile(&mut restate, &mut RecordedRetirements::default())
            .await
            .unwrap();

        assert_eq!(
            restate.retained,
            Some(vec![]),
            "every pull request closed means every stale row goes"
        );
    }
}
