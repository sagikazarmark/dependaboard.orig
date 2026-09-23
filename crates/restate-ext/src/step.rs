//! Journaling a fallible effect as a Restate step: how it is named, how it retries and how
//! its failures read.

use std::{fmt, time::Duration};

use restate_sdk::prelude::*;
// Restate's own serde traits, which `ctx.run` journals its result through; not the
// `serde` crate's, which a journaled value reaches by going through `Json`.
use restate_sdk::serde::{Deserialize, Serialize};

use crate::handler::RetryableStepError;

/// What an effect's own error says about a fresh attempt: whether the failure is expected
/// to clear on its own, or whether the same call would meet the same thing again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    /// Transient: contention, a dropped connection, a 5xx, a restart. Retrying the
    /// identical call may succeed.
    Retryable,
    /// A fresh attempt would meet the same: a schema that does not match, a payload the
    /// far end refuses, a bug.
    Terminal,
}

/// An error that says which class of failure it is, so a [`StepKind`] can decide what
/// Restate should make of it.
///
/// Implemented on the error type of whatever the step calls — a store, a queue, a service
/// client. `Display` is the bound rather than `std::error::Error` because the message is
/// all a journaled failure carries: it is what an operator reads in Restate's UI.
pub trait ClassifiedError: fmt::Display {
    fn class(&self) -> FailureClass;
}

/// A Restate context a step can be journaled on.
///
/// Every effect a handler runs goes through [`Self::run_step`], so none is journaled
/// without a name to find it by in Restate's UI or a [`StepKind`] chosen on purpose — a
/// bare `ctx.run` would retry under the server's default, indefinitely.
///
/// A trait with an impl per context rather than one function over `ContextSideEffects`:
/// the SDK does not promise its `run` future is `Send`, and a function generic over the
/// context cannot see through to the future that is, so each impl names its context and
/// lets the compiler look.
pub trait StepContext<'ctx> {
    /// Journals one step under `name`, as the kind of effect `kind` says it is.
    ///
    /// The kind carries both halves of what the step needs from Restate — the retry budget
    /// it is journaled under and how a failure reads to Restate — so the two cannot be
    /// picked apart at a call site; see [`StepKind`]. `step` fails with the effect's own
    /// error and this reads it, through [`ClassifiedError`]. What it resolves to is
    /// journaled, so a value goes through [`Json`]. Resolves to what the step did, or the
    /// terminal failure Restate ends it with once the kind's budget is spent.
    fn run_step<F, T, E>(
        &self,
        name: &'static str,
        kind: StepKind,
        step: impl FnOnce() -> F + Send + 'ctx,
    ) -> impl Future<Output = Result<T, TerminalError>> + Send
    where
        F: Future<Output = Result<T, E>> + Send + 'ctx,
        T: Serialize + Deserialize + 'static,
        E: ClassifiedError + Send + 'ctx;
}

/// One impl per context, written out by the macro because the bodies are identical and
/// only the named type differs — see the note on [`StepContext`] for why they cannot be
/// one generic impl.
macro_rules! impl_step_context {
    ($($context:ident),+ $(,)?) => {
        $(
            impl<'ctx> StepContext<'ctx> for $context<'ctx> {
                fn run_step<F, T, E>(
                    &self,
                    name: &'static str,
                    kind: StepKind,
                    step: impl FnOnce() -> F + Send + 'ctx,
                ) -> impl Future<Output = Result<T, TerminalError>> + Send
                where
                    F: Future<Output = Result<T, E>> + Send + 'ctx,
                    T: Serialize + Deserialize + 'static,
                    E: ClassifiedError + Send + 'ctx,
                {
                    self.run(move || async move {
                        step().await.map_err(|error| kind.read_failure(&error))
                    })
                    .retry_policy(kind.retry_policy())
                    .name(name)
                }
            }
        )+
    };
}

impl_step_context!(
    Context,
    ObjectContext,
    SharedObjectContext,
    WorkflowContext,
    SharedWorkflowContext,
);

/// What kind of effect a step is: whether anything later would redo it, and whether the
/// work waits on it. One choice rather than two, because the retry budget and the reading
/// of a failure only make sense together — a budget that never gives up under a reading
/// that ends the step on the callee's word would give up after all, and the one effect
/// nothing would redo would lose its record. Each kind answers both, through
/// [`Self::retry_policy`] and [`Self::read_failure`], and a call site names only the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// An effect something later would redo: a write a sweep or a reconcile would write
    /// again, a read a retry would take again. The failures such a step meets — contention,
    /// a race with a row that does not exist yet, a remote blip such as a dropped stream, a
    /// 5xx or a restart — resolve within seconds, so it backs off for up to five minutes
    /// and then gives up with a terminal failure. Nothing is lost by giving up: whatever
    /// would have redone the work still will.
    Ordinary,
    /// A convenience that stands in the way of the work: a progress row that lets an audit
    /// view show what is running, say, which should not hold up the hundred operations it
    /// describes. The budget is cut to seconds and the caller carries on without it once it
    /// is spent.
    Brief,
    /// The effect nothing would redo: a record written by the one run a workflow key gets,
    /// so giving up would lose it for good. It retries until the callee takes it, with no
    /// budget and whatever the callee said of the failure, and the invocation stalls in
    /// plain sight until an operator has put things right.
    LastChance,
}

impl StepKind {
    /// The backoff this kind is journaled under. All three climb from 100ms to five
    /// seconds — the failures these steps meet clear in that range or not at all — and
    /// differ only in how long they keep at it.
    pub fn retry_policy(self) -> RunRetryPolicy {
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

    /// How a failure reads to Restate for this kind of effect.
    ///
    /// For an effect something later would redo the callee has the word: retried if it says
    /// the failure will clear, ended if it says a fresh attempt would meet the same.
    /// [`Self::LastChance`] inverts that, because it has no later chance: a terminal-class
    /// failure is retried too, under a budget that does not end. The class is kept in the
    /// message either way, so the failure Restate shows says whether the callee expected it
    /// to clear on its own.
    pub fn read_failure<E: ClassifiedError>(self, error: &E) -> HandlerError {
        match (self, error.class()) {
            (_, FailureClass::Retryable) => RetryableStepError::Transient(error.to_string()).into(),
            (Self::Ordinary | Self::Brief, FailureClass::Terminal) => {
                TerminalError::new(error.to_string()).into()
            }
            (Self::LastChance, FailureClass::Terminal) => {
                RetryableStepError::RetriedRegardless(error.to_string()).into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error whose class a test picks, standing in for whatever the step calls.
    struct Failure(FailureClass);

    impl fmt::Display for Failure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("the callee said no")
        }
    }

    impl ClassifiedError for Failure {
        fn class(&self) -> FailureClass {
            self.0
        }
    }

    fn cause(error: &HandlerError) -> String {
        let cause: &dyn std::error::Error = error.as_ref();
        cause.to_string()
    }

    /// The kind is the whole choice: each one carries the reading that belongs with its
    /// budget, so no call site can journal an effect under one and read its failures by
    /// the other. The classifier's word is final for an effect something later would redo
    /// and for the convenience that stands in the way of the work: a terminal-class
    /// failure ends the step. The record nothing would redo has no later chance, so for it
    /// the same failure is retried, and the message says the callee did not expect the
    /// failure to clear rather than calling it transient, so an operator reading Restate's
    /// failure knows to look at the callee, not wait it out.
    #[test]
    fn each_kind_reads_a_terminal_class_error_the_way_its_budget_needs() {
        for (kind, expected) in [
            (StepKind::Ordinary, "Terminal error"),
            (StepKind::Brief, "Terminal error"),
            (
                StepKind::LastChance,
                "Retryable error: step met a terminal-class failure; retrying regardless: ",
            ),
        ] {
            let read = cause(&kind.read_failure(&Failure(FailureClass::Terminal)));
            assert!(read.starts_with(expected), "{kind:?}: {read}");
        }
    }

    /// A failure the callee expects to clear is retried whatever the kind, so the step's
    /// budget is what decides how long — not a reading that differs per kind.
    #[test]
    fn every_kind_retries_a_retryable_class_error() {
        for kind in [StepKind::Ordinary, StepKind::Brief, StepKind::LastChance] {
            let read = cause(&kind.read_failure(&Failure(FailureClass::Retryable)));
            assert!(
                read.starts_with("Retryable error: transient failure: "),
                "{kind:?}: {read}"
            );
        }
    }
}
