//! Maps projection-store effects onto Restate: how a store step is journaled, how it
//! retries and how its failures read.

use std::time::Duration;

use dependaboard_store::{StoreError, StoreErrorClass};
use restate_sdk::prelude::*;
// Restate's own serde traits, which `ctx.run` journals its result through; not the
// `serde` crate's, which a journaled value reaches by going through `Json`.
use restate_sdk::serde::{Deserialize, Serialize};

use crate::handler::RetryableServiceError;

/// A Restate context a store step can be journaled on: the object and workflow contexts,
/// which are the ones that write the projection.
///
/// Every store call a handler makes goes through [`Self::run_store_step`], so none is
/// journaled without a name to find it by in Restate's UI or a retry policy chosen on
/// purpose — a bare `ctx.run` would retry under the server's default, indefinitely.
///
/// A trait with an impl per context rather than one function over `ContextSideEffects`:
/// the SDK does not promise its `run` future is `Send`, and a function generic over the
/// context cannot see through to the future that is, so each impl names its context and
/// lets the compiler look.
pub(crate) trait StoreStepContext<'ctx> {
    /// Journals one store step under `name`, retried under `policy`.
    ///
    /// The caller picks the policy by name: [`store_retry_policy`] for a write something
    /// later would redo, [`brief_store_retry_policy`] for a convenience that stands in
    /// the way of the work, and [`persistent_store_retry_policy`] for the one write
    /// nothing would redo. `step` says how its own failures read, with [`store_failure`]
    /// or [`batch_record_failure`]; what it resolves to is journaled, so a value goes
    /// through [`Json`]. Resolves to what the step did, or the terminal failure Restate
    /// ends it with once the policy's budget is spent.
    fn run_store_step<F, T>(
        &self,
        name: &'static str,
        policy: RunRetryPolicy,
        step: impl FnOnce() -> F + Send + 'ctx,
    ) -> impl Future<Output = Result<T, TerminalError>> + Send
    where
        F: Future<Output = HandlerResult<T>> + Send + 'ctx,
        T: Serialize + Deserialize + 'static;
}

impl<'ctx> StoreStepContext<'ctx> for ObjectContext<'ctx> {
    fn run_store_step<F, T>(
        &self,
        name: &'static str,
        policy: RunRetryPolicy,
        step: impl FnOnce() -> F + Send + 'ctx,
    ) -> impl Future<Output = Result<T, TerminalError>> + Send
    where
        F: Future<Output = HandlerResult<T>> + Send + 'ctx,
        T: Serialize + Deserialize + 'static,
    {
        self.run(step).retry_policy(policy).name(name)
    }
}

impl<'ctx> StoreStepContext<'ctx> for WorkflowContext<'ctx> {
    fn run_store_step<F, T>(
        &self,
        name: &'static str,
        policy: RunRetryPolicy,
        step: impl FnOnce() -> F + Send + 'ctx,
    ) -> impl Future<Output = Result<T, TerminalError>> + Send
    where
        F: Future<Output = HandlerResult<T>> + Send + 'ctx,
        T: Serialize + Deserialize + 'static,
    {
        self.run(step).retry_policy(policy).name(name)
    }
}

/// Bounded backoff for projection-store writes: SQLite contention, the FK race between a
/// fresh repository's first webhook and its enumeration, and a remote store's blip — a
/// dropped stream, a 5xx, a restart — resolve within seconds, so back off from 100ms to
/// five seconds and give up after five minutes with a terminal failure. A webhook that
/// outlives the budget is not lost: the next reconcile syncs the same pull request once
/// its repository row exists.
pub(crate) fn store_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(100))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5))
        .max_duration(Duration::from_secs(5 * 60))
}

/// The same backoff without the budget, for a projection write nothing would redo: a
/// finished batch's record is written by the one run its workflow key gets, so giving up
/// would lose the record for good. The step retries until the store takes it — paired with
/// [`batch_record_failure`], so no failure of the store's ends it early; the batch's
/// progress is already published, so the dashboard is not kept waiting on it.
pub(crate) fn persistent_store_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(100))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5))
}

/// The same backoff cut short, for a projection write that is a convenience and stands
/// in the way of the work: listing a batch as running is how the audit view shows it
/// before it finishes, and a store away for longer than this should not hold up a
/// hundred merges. The caller carries on without the write once the budget is spent.
pub(crate) fn brief_store_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(100))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5))
        .max_duration(Duration::from_secs(15))
}

/// How a projection-store failure reads to Restate: retried if the store says the failure
/// will clear, ended if it says a fresh attempt would meet the same. Right for every write
/// something later would redo — the next webhook, the next reconcile — and wrong for the
/// one that nothing would, which has [`batch_record_failure`].
pub(crate) fn store_failure(error: StoreError) -> HandlerError {
    match error.class() {
        StoreErrorClass::Retryable => RetryableServiceError::Store(error.to_string()).into(),
        StoreErrorClass::Terminal => TerminalError::new(error.to_string()).into(),
    }
}

/// How a failure to write the finished batch's record reads to Restate: retryable,
/// whatever the store said. [`store_failure`] lets a terminal-class error end the step,
/// because every other write has a later chance; this one has none, and a record given
/// up on is an audit row lost while the merges it describes stand on GitHub. So a schema
/// that does not match, a disk that is full, or a `libsql` error the classifier does not
/// know are all retried under [`persistent_store_retry_policy`], and the workflow stalls
/// in plain sight until an operator has put the store right. The store's class is kept in
/// the message, so the failure Restate shows says whether the store expected it to clear
/// on its own.
pub(crate) fn batch_record_failure(error: StoreError) -> HandlerError {
    match error.class() {
        StoreErrorClass::Retryable => store_failure(error),
        StoreErrorClass::Terminal => {
            RetryableServiceError::StoreRefusedRecord(error.to_string()).into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cause(error: &HandlerError) -> String {
        let cause: &dyn std::error::Error = error.as_ref();
        cause.to_string()
    }

    /// The classifier's word is final for a write something later would redo: a
    /// terminal-class failure ends the step. The finished batch's record has no later
    /// chance, so for it the same failure is retried, and the message says the store did
    /// not expect the failure to clear rather than calling it transient, so an operator
    /// reading Restate's failure knows to look at the store, not wait it out.
    #[test]
    fn a_terminal_class_store_error_ends_an_ordinary_write_but_not_the_batch_record() {
        let ordinary = cause(&store_failure(StoreError::MissingScalar));
        assert!(ordinary.starts_with("Terminal error"), "{ordinary}");

        let record = cause(&batch_record_failure(StoreError::MissingScalar));
        assert!(
            record.starts_with(
                "Retryable error: batch record met a terminal-class store error; retrying regardless: "
            ),
            "{record}"
        );
    }
}
