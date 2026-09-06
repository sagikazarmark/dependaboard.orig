//! The retirement outbox's Restate side: telling every pull request whose row a prune
//! removed that it is gone, from what the store queued rather than from what the prune's
//! step returned.
//!
//! A prune runs inside `ctx.run`, and a step's result can be lost after its effect has
//! landed: the delete commits, the process dies before the journal takes the returned
//! keys, and the re-run finds nothing left to delete. Driving the `closed` sends from
//! that return value would leave the objects that just lost their rows serving a
//! snapshot the projection no longer has, indefinitely. So the store queues the keys in
//! the delete's own transaction, and the sweep drains the queue afterwards in steps that
//! are each safe to run again: a re-run read sees the same rows, a duplicate `closed` is
//! idempotent, and a re-run acknowledgement is a no-op.

use std::sync::Arc;

use dependaboard_core::{PrKey, Retirement};
use dependaboard_store::PrStore;
use restate_sdk::prelude::*;

use crate::{
    pull_request::{ClosedRequest, close_pull_request},
    store::{store_failure, store_retry_policy},
};

/// Side effects draining the outbox asks of Restate and the store, abstracted so
/// `retire_pending` can be exercised against a recording fake without a runtime.
pub(crate) trait RetirementEffects {
    /// Every pull request a prune has removed and not yet acknowledged, oldest first.
    fn pending_retirements(
        &mut self,
    ) -> impl Future<Output = HandlerResult<Vec<Retirement>>> + Send;
    /// Sends `PullRequest.closed` to the pull request's object, one-way, with `request`
    /// as `retire_pending` built it from the retirement.
    fn close_pull_request(&mut self, key: &PrKey, request: ClosedRequest);
    /// Forgets every retirement up to and including `through`, the last one read.
    fn acknowledge_retirements(
        &mut self,
        through: u64,
    ) -> impl Future<Output = HandlerResult<()>> + Send;
}

/// The Restate-bound outbox: each read and acknowledgement is a journaled store step.
pub(crate) struct RestateRetirements<'a, 'ctx> {
    pub(crate) ctx: &'a ObjectContext<'ctx>,
    pub(crate) store: &'a Arc<dyn PrStore>,
}

impl RetirementEffects for RestateRetirements<'_, '_> {
    async fn pending_retirements(&mut self) -> HandlerResult<Vec<Retirement>> {
        let store = self.store.clone();
        let pending = self
            .ctx
            .run(move || async move {
                Ok(Json::from(
                    store.pending_retirements().await.map_err(store_failure)?,
                ))
            })
            .retry_policy(store_retry_policy())
            .name("read-pending-retirements")
            .await?;
        Ok(pending.into_inner())
    }

    fn close_pull_request(&mut self, key: &PrKey, request: ClosedRequest) {
        close_pull_request(self.ctx, key, request);
    }

    async fn acknowledge_retirements(&mut self, through: u64) -> HandlerResult<()> {
        let store = self.store.clone();
        self.ctx
            .run(move || async move {
                store
                    .acknowledge_retirements(through)
                    .await
                    .map_err(store_failure)?;
                Ok(())
            })
            .retry_policy(store_retry_policy())
            .name("acknowledge-retirements")
            .await?;
        Ok(())
    }
}

/// Tells every pull request the projection has pruned and not yet told that it is gone,
/// then forgets them. Resolves to how many were told.
///
/// Run it after a prune, whatever the prune's step returned: the outbox holds the keys
/// of this prune and of any earlier one whose drain did not complete. Each close carries
/// the fence its own prune recorded — the instant that sweep's listing started, so an
/// object synced since was reopened behind it and keeps its state — never the drain's
/// clock. The acknowledgement covers exactly what was read; anything queued in between
/// waits for the next drain.
pub(crate) async fn retire_pending<E: RetirementEffects>(restate: &mut E) -> HandlerResult<usize> {
    let pending = restate.pending_retirements().await?;
    let Some(last) = pending.last() else {
        return Ok(0);
    };
    for retirement in &pending {
        restate.close_pull_request(
            &retirement.key,
            ClosedRequest {
                synced_before: retirement.synced_before,
            },
        );
    }
    restate.acknowledge_retirements(last.id).await?;
    Ok(pending.len())
}

#[cfg(test)]
mod tests {
    use dependaboard_core::PrKey;

    use super::*;
    use crate::test_support::RecordedRetirements;

    fn retirement(id: u64, key: PrKey, synced_before: Option<u64>) -> Retirement {
        Retirement {
            id,
            key,
            synced_before,
        }
    }

    #[tokio::test]
    async fn every_pending_retirement_is_closed_under_its_own_fence_and_then_acknowledged() {
        let mut restate = RecordedRetirements {
            pending: vec![
                retirement(3, PrKey::new(7, 3), Some(900)),
                retirement(4, PrKey::new(8, 1), None),
                retirement(9, PrKey::new(7, 9), Some(1_000)),
            ],
            ..Default::default()
        };

        let retired = retire_pending(&mut restate).await.unwrap();

        assert_eq!(retired, 3);
        assert_eq!(
            restate.closed,
            vec![
                (
                    PrKey::new(7, 3),
                    ClosedRequest {
                        synced_before: Some(900)
                    }
                ),
                (PrKey::new(8, 1), ClosedRequest::default()),
                (
                    PrKey::new(7, 9),
                    ClosedRequest {
                        synced_before: Some(1_000)
                    }
                ),
            ],
            "each close carries the fence its prune ran under, not the drain's clock"
        );
        assert_eq!(
            restate.acknowledged,
            vec![9],
            "the read is acknowledged through its last row, once every close is sent"
        );
    }

    #[tokio::test]
    async fn an_empty_outbox_acknowledges_nothing() {
        let mut restate = RecordedRetirements::default();

        let retired = retire_pending(&mut restate).await.unwrap();

        assert_eq!(retired, 0);
        assert!(restate.closed.is_empty());
        assert!(restate.acknowledged.is_empty());
    }

    #[tokio::test]
    async fn a_failed_read_closes_nothing() {
        let mut restate = RecordedRetirements {
            pending: vec![retirement(3, PrKey::new(7, 3), Some(900))],
            read_failure: Some(TerminalError::new("projection store is read-only").into()),
            ..Default::default()
        };

        let outcome = retire_pending(&mut restate).await;

        assert!(outcome.is_err(), "the failed read stays visible to Restate");
        assert!(
            restate.closed.is_empty(),
            "what is queued is unknown, so nothing is told and nothing is forgotten"
        );
        assert!(restate.acknowledged.is_empty());
    }
}
