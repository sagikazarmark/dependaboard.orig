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

pub(crate) fn store_failure(error: StoreError) -> HandlerError {
    match error.class() {
        StoreErrorClass::Retryable => RetryableServiceError::Store(error.to_string()).into(),
        StoreErrorClass::Terminal => TerminalError::new(error.to_string()).into(),
    }
}
