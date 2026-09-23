//! Reading the clock as a journaled step.

use dependaboard_core::unix_seconds;
use restate_sdk::prelude::*;

/// A Restate context the clock can be read on: the object and workflow contexts, which
/// are the ones whose handlers stamp what they did with a time.
///
/// A clock read is a step like any other, and for the reason every step is one: the
/// reading is journaled, so a replay stamps what the first run stamped rather than
/// reading the wall clock again and dating the same work twice. `name` is what a replay
/// matches the entry by, so two reads in one invocation need two names — which is why
/// this takes a name at all rather than being `unix_seconds()` at the call site.
///
/// A trait with an impl per context rather than one function over `ContextSideEffects`,
/// for the reason [`StoreStepContext`](crate::store::StoreStepContext) gives: the SDK
/// does not promise its `run` future is `Send`, and a function generic over the context
/// cannot see through to the future that is, so each impl names its context and lets the
/// compiler look.
pub(crate) trait ClockStepContext<'ctx> {
    /// Journals one clock read under `name`, resolving to the second it read.
    fn run_clock_step(
        &self,
        name: &'static str,
    ) -> impl Future<Output = Result<u64, TerminalError>> + Send;
}

impl<'ctx> ClockStepContext<'ctx> for ObjectContext<'ctx> {
    fn run_clock_step(
        &self,
        name: &'static str,
    ) -> impl Future<Output = Result<u64, TerminalError>> + Send {
        self.run(|| async { Ok(unix_seconds()) }).name(name)
    }
}

impl<'ctx> ClockStepContext<'ctx> for WorkflowContext<'ctx> {
    fn run_clock_step(
        &self,
        name: &'static str,
    ) -> impl Future<Output = Result<u64, TerminalError>> + Send {
        self.run(|| async { Ok(unix_seconds()) }).name(name)
    }
}
