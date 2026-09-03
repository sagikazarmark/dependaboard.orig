//! The `BulkAction` workflow: one per dashboard batch, driving a merge or rebase across
//! many pull requests and exposing its progress to the dashboard.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Duration,
};

use dependaboard_core::{
    BatchProgress, BulkActionKind, BulkRequest, CommandRequest, DependabotCommand,
    MAX_BATCH_TARGETS, MergeRequest, TargetProgressState, valid_batch_id,
};
use restate_sdk::prelude::*;

use crate::{
    handler::{HandlerOutcome, traced, traced_read},
    pull_request::PullRequestClient,
};

const BATCH_PROGRESS: &str = "progress";
const MAX_CONCURRENT: usize = 3;

pub(crate) struct BulkAction;

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

#[cfg(test)]
mod tests {
    use dependaboard_core::UserId;

    use super::*;
    use crate::test_support::target;

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
}
