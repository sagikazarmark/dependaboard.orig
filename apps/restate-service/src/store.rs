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
/// journaled without a name to find it by in Restate's UI or a [`StoreStepKind`] chosen on
/// purpose — a bare `ctx.run` would retry under the server's default, indefinitely.
///
/// A trait with an impl per context rather than one function over `ContextSideEffects`:
/// the SDK does not promise its `run` future is `Send`, and a function generic over the
/// context cannot see through to the future that is, so each impl names its context and
/// lets the compiler look.
pub(crate) trait StoreStepContext<'ctx> {
    /// Journals one store step under `name`, as the kind of write `kind` says it is.
    ///
    /// The kind carries both halves of what the step needs from Restate — the retry budget
    /// it is journaled under and how a failure of the store's reads to Restate — so the two
    /// cannot be picked apart at a call site; see [`StoreStepKind`]. `step` fails with the
    /// store's own [`StoreError`] and this reads it. What it resolves to is journaled, so a
    /// value goes through [`Json`]. Resolves to what the step did, or the terminal failure
    /// Restate ends it with once the kind's budget is spent.
    fn run_store_step<F, T>(
        &self,
        name: &'static str,
        kind: StoreStepKind,
        step: impl FnOnce() -> F + Send + 'ctx,
    ) -> impl Future<Output = Result<T, TerminalError>> + Send
    where
        F: Future<Output = Result<T, StoreError>> + Send + 'ctx,
        T: Serialize + Deserialize + 'static;
}

impl<'ctx> StoreStepContext<'ctx> for ObjectContext<'ctx> {
    fn run_store_step<F, T>(
        &self,
        name: &'static str,
        kind: StoreStepKind,
        step: impl FnOnce() -> F + Send + 'ctx,
    ) -> impl Future<Output = Result<T, TerminalError>> + Send
    where
        F: Future<Output = Result<T, StoreError>> + Send + 'ctx,
        T: Serialize + Deserialize + 'static,
    {
        self.run(move || async move { step().await.map_err(|error| kind.read_failure(&error)) })
            .retry_policy(kind.retry_policy())
            .name(name)
    }
}

impl<'ctx> StoreStepContext<'ctx> for WorkflowContext<'ctx> {
    fn run_store_step<F, T>(
        &self,
        name: &'static str,
        kind: StoreStepKind,
        step: impl FnOnce() -> F + Send + 'ctx,
    ) -> impl Future<Output = Result<T, TerminalError>> + Send
    where
        F: Future<Output = Result<T, StoreError>> + Send + 'ctx,
        T: Serialize + Deserialize + 'static,
    {
        self.run(move || async move { step().await.map_err(|error| kind.read_failure(&error)) })
            .retry_policy(kind.retry_policy())
            .name(name)
    }
}

/// What kind of write a store step is: whether anything later would redo it, and whether
/// the work waits on it. One choice rather than two, because the retry budget and the
/// reading of a failure only make sense together — a budget that never gives up under a
/// reading that ends the step on the store's word would give up after all, and the one
/// write nothing would redo would lose its record. Each kind answers both, through
/// [`Self::retry_policy`] and [`Self::read_failure`], and a call site names only the kind.
#[derive(Clone, Copy, Debug)]
pub(crate) enum StoreStepKind {
    /// A write something later would redo — a webhook's upsert, a sweep's prune, a drain's
    /// read, the unlisting a failed or cancelled workflow does on its way out. SQLite
    /// contention, the FK race between a fresh repository's first webhook and its
    /// enumeration, and a remote store's blip — a dropped stream, a 5xx, a restart —
    /// resolve within seconds, so it backs off for up to five minutes and then gives up
    /// with a terminal failure. A webhook that outlives the budget is not lost: the next
    /// reconcile syncs the same pull request once its repository row exists.
    Ordinary,
    /// A convenience that stands in the way of the work: listing a batch as running is how
    /// the audit view shows it before it finishes, and a store away for longer than this
    /// should not hold up a hundred merges. The budget is cut to seconds and the caller
    /// carries on without the write once it is spent.
    Brief,
    /// The write nothing would redo: a finished batch's record is written by the one run
    /// its workflow key gets, so giving up would lose the record for good, and a record
    /// given up on is an audit row lost while the merges it describes stand on GitHub. It
    /// retries until the store takes it, with no budget and whatever the store said of the
    /// failure, and the workflow stalls in plain sight until an operator has put the store
    /// right; the batch's progress is already published, so the dashboard is not kept
    /// waiting on it.
    LastChance,
}

impl StoreStepKind {
    /// The backoff this kind is journaled under. All three climb from 100ms to five
    /// seconds — the failures a projection write meets clear in that range or not at all —
    /// and differ only in how long they keep at it.
    pub(crate) fn retry_policy(self) -> RunRetryPolicy {
        let backoff = RunRetryPolicy::new()
            .initial_delay(Duration::from_millis(100))
            .exponentiation_factor(2.0)
            .max_delay(Duration::from_secs(5));
        match self {
            Self::Ordinary => backoff.max_duration(Duration::from_secs(5 * 60)),
            Self::Brief => backoff.max_duration(Duration::from_secs(15)),
            // No budget: see the variant.
            Self::LastChance => backoff,
        }
    }

    /// How a failure of the store's reads to Restate for this kind of write.
    ///
    /// For a write something later would redo the store has the word: retried if it says
    /// the failure will clear, ended if it says a fresh attempt would meet the same.
    /// [`Self::LastChance`] inverts that, because it has no later chance: a schema that
    /// does not match, a disk that is full, or a `libsql` error the classifier does not
    /// know are all retried, under a budget that does not end. The store's class is kept in
    /// the message either way, so the failure Restate shows says whether the store expected
    /// it to clear on its own.
    pub(crate) fn read_failure(self, error: &StoreError) -> HandlerError {
        match (self, error.class()) {
            (_, StoreErrorClass::Retryable) => {
                RetryableServiceError::Store(error.to_string()).into()
            }
            (Self::Ordinary | Self::Brief, StoreErrorClass::Terminal) => {
                TerminalError::new(error.to_string()).into()
            }
            (Self::LastChance, StoreErrorClass::Terminal) => {
                RetryableServiceError::StoreRefusedRecord(error.to_string()).into()
            }
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

    /// The kind is the whole choice: each one carries the reading that belongs with its
    /// budget, so no call site can journal a write under one and read its failures by the
    /// other. The classifier's word is final for a write something later would redo and
    /// for the convenience that stands in the way of the work: a terminal-class failure
    /// ends the step. The finished batch's record has no later chance, so for it the same
    /// failure is retried, and the message says the store did not expect the failure to
    /// clear rather than calling it transient, so an operator reading Restate's failure
    /// knows to look at the store, not wait it out.
    #[test]
    fn each_kind_reads_a_terminal_class_store_error_the_way_its_budget_needs() {
        for (kind, expected) in [
            (StoreStepKind::Ordinary, "Terminal error"),
            (StoreStepKind::Brief, "Terminal error"),
            (
                StoreStepKind::LastChance,
                "Retryable error: batch record met a terminal-class store error; retrying regardless: ",
            ),
        ] {
            let read = cause(&kind.read_failure(&StoreError::MissingScalar));
            assert!(read.starts_with(expected), "{kind:?}: {read}");
        }
    }
}
