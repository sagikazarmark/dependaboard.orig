//! The `RepoSync` virtual object: one per repository, sweeping its open Dependabot pull
//! requests and fanning commit-status changes out to the pull requests they touch.

use std::{collections::BTreeSet, sync::Arc};

use dependaboard_core::{Operation, RepoRecord, SyncRequest, SyncShaRequest, unix_seconds};
use dependaboard_store::ProjectionWriter;
use restate_sdk::prelude::*;
use tracing::warn;

use crate::{
    github::{GithubApiHandle, RestateGithubStep, read_result, run_github_step},
    handler::traced,
    pull_request::{PullRequestClient, request_key, short_sha},
    retirement::{RestateRetirements, RetirementEffects, retire_pending},
    store::{StoreStepContext, store_failure, store_retry_policy},
};

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
    /// Prunes the projection down to `live`. The keys it removes are queued for
    /// retirement by the store, fenced by the sweep's start; the drain that follows tells
    /// them, so nothing here depends on what this step returns.
    fn retain_pull_requests(
        &mut self,
        live: &[u64],
    ) -> impl Future<Output = HandlerResult<()>> + Send;
}

struct RestateReconcileEffects<'a, 'ctx> {
    ctx: &'a ObjectContext<'ctx>,
    github: &'a GithubApiHandle,
    store: &'a Arc<dyn ProjectionWriter>,
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
            .run_store_step(
                "retain-live-pull-requests",
                store_retry_policy(),
                move || async move {
                    store
                        .retain_prs(repository_id, &live, reconcile_start)
                        .await
                        .map(drop)
                        .map_err(store_failure)
                },
            )
            .await?;
        Ok(())
    }
}

/// Sweeps every listed pull request, prunes the projection down to the listing, then
/// retires the durable state of every pull request the outbox holds — those pruning
/// removed just now, and any an earlier prune removed without getting to tell.
///
/// A pull request that fails terminally is logged and remembered rather than propagated,
/// so one unsyncable pull request can neither starve the rest of the repository nor skip
/// stale-row cleanup. Retention still requires a complete listing: if listing fails, the
/// live set is unknown and nothing is deleted. Resolves to how many pull requests synced.
async fn run_repo_reconcile<E: RepoReconcileEffects, R: RetirementEffects>(
    restate: &mut E,
    retirements: &mut R,
) -> HandlerResult<usize> {
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
    retire_pending(retirements).await?;
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
            let store = self.store.clone();
            let repository_id = request.repository_id;
            let sha = request.sha.clone();
            let matches = ctx
                .run_store_step(
                    "resolve-prs-for-sha",
                    store_retry_policy(),
                    move || async move {
                        store
                            .prs_for_sha(repository_id, &sha)
                            .await
                            .map(Json::from)
                            .map_err(store_failure)
                    },
                )
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dependaboard_core::PrKey;

    use super::*;
    use crate::{pull_request::ClosedRequest, test_support::RecordedRetirements};

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
    async fn every_pruned_pull_request_has_its_durable_state_closed_under_the_sweeps_fence() {
        let mut restate = RecordedRepoSync {
            pulls: vec![dependabot_pull(12)],
            ..Default::default()
        };
        // What the projection queued when it pruned 3 and 9, whether by this run of the
        // retain or by an earlier attempt whose result never reached the journal.
        let mut retirements =
            RecordedRetirements::queued(&[PrKey::new(7, 3), PrKey::new(7, 9)], Some(900));

        run_repo_reconcile(&mut restate, &mut retirements)
            .await
            .unwrap();

        assert_eq!(restate.retained, Some(vec![12]));
        assert_eq!(
            retirements.closed,
            vec![
                (
                    PrKey::new(7, 3),
                    ClosedRequest {
                        synced_before: Some(900)
                    }
                ),
                (
                    PrKey::new(7, 9),
                    ClosedRequest {
                        synced_before: Some(900)
                    }
                ),
            ],
            "a row pruned from the projection must not keep a snapshot in its object, \
             unless it was re-synced since the sweep began"
        );
        assert_eq!(retirements.acknowledged, vec![2]);
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

        assert_eq!(restate.synced, vec![12, 19, 23]);
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
        assert_eq!(restate.synced, vec![12, 19]);
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
