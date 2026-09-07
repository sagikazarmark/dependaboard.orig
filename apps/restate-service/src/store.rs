//! Maps projection-store effects onto Restate: how a store write retries and how its
//! failures read.

use std::time::Duration;

use dependaboard_store::{StoreError, StoreErrorClass};
use restate_sdk::prelude::*;

use crate::handler::RetryableServiceError;

/// Bounded backoff for projection-store writes: SQLite contention and the FK race between
/// a fresh repository's first webhook and its enumeration resolve within seconds, so back
/// off from 100ms to five seconds and give up after five minutes with a terminal failure.
/// A webhook that outlives the budget is not lost: the next reconcile syncs the same pull
/// request once its repository row exists.
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
/// that does not match, a disk that is full, or a remote store whose failure the
/// classifier does not know are all retried under [`persistent_store_retry_policy`], and
/// the workflow stalls in plain sight until an operator has put the store right. The
/// store's class is kept in the message, so the failure Restate shows says whether the
/// store expected it to clear on its own.
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
