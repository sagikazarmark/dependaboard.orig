//! The `PullRequest` virtual object: one per Dependabot pull request, owning its canonical
//! snapshot, its action history, and every GitHub mutation made on its behalf.

use std::time::Duration;

use dependaboard_core::{
    ActionLog, ActionOutcome, CommandRequest, MergeRequest, Operation, PrKey, PrState,
    RejectReason, SyncRequest, UpdateBranchRequest, unix_seconds,
};
use dependaboard_github::{GithubApi, GithubClient};
use dependaboard_store::{LibSqlPrStore, PrStore};
use restate_sdk::prelude::*;

use crate::{
    github::{RestateGithubStep, Settled, action_result, read_result, rejected, run_github_step},
    handler::{HandlerOutcome, traced, traced_read},
    store::{store_failure, store_retry_policy},
};

const PR_STATE: &str = "pr_state";

#[derive(Clone)]
pub(crate) struct PullRequest {
    pub(crate) github: GithubClient,
    pub(crate) store: LibSqlPrStore,
    pub(crate) debounce: Duration,
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

fn should_debounce_sync(
    bypass_debounce: bool,
    last_synced_at: Option<u64>,
    now: u64,
    debounce: Duration,
) -> bool {
    !bypass_debounce
        && last_synced_at.is_some_and(|last| now.saturating_sub(last) < debounce.as_secs())
}

fn outcome_detail(outcome: &ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Succeeded { detail } => detail.clone(),
        ActionOutcome::Rejected { reason } => reason.to_string(),
    }
}

/// The key of the `PullRequest` object a sync request addresses.
pub(crate) fn request_key(request: &SyncRequest) -> PrKey {
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

pub(crate) fn short_sha(value: &str) -> &str {
    value.get(..7).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{CheckStatus, Mergeable, PrRecord, UpdateType};
    use restate_sdk::service::Discoverable;

    use super::*;
    use crate::test_support::target;

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

    #[test]
    fn dashboard_sync_bypasses_the_event_debounce() {
        let debounce = Duration::from_secs(20);
        assert!(should_debounce_sync(false, Some(100), 101, debounce));
        assert!(!should_debounce_sync(true, Some(100), 101, debounce));
    }
}
