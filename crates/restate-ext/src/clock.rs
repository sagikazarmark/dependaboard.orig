//! Reading the clock as a journaled step.

use restate_sdk::prelude::*;

/// Seconds since the Unix epoch on the host clock.
///
/// The reading a [`ClockStepContext`] journals. Public because a handler that already has
/// a time to stamp — one a caller passed in, say — should stamp it with the same unit the
/// journaled reads use.
pub fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A Restate context the clock can be read on.
///
/// A clock read is a step like any other, and for the reason every step is one: the
/// reading is journaled, so a replay stamps what the first run stamped rather than
/// reading the wall clock again and dating the same work twice. `name` is what a replay
/// matches the entry by, so two reads in one invocation need two names — which is why
/// this takes a name at all rather than being [`unix_seconds`] at the call site.
///
/// A trait with an impl per context rather than one function over `ContextSideEffects`,
/// for the reason [`StepContext`](crate::step::StepContext) gives: the SDK does not
/// promise its `run` future is `Send`, and a function generic over the context cannot see
/// through to the future that is, so each impl names its context and lets the compiler
/// look.
pub trait ClockStepContext<'ctx> {
    /// Journals one clock read under `name`, resolving to the second it read.
    fn run_clock_step(
        &self,
        name: &'static str,
    ) -> impl Future<Output = Result<u64, TerminalError>> + Send;
}

/// One impl per context, written out by the macro because the bodies are identical and
/// only the named type differs — see the note on [`ClockStepContext`] for why they cannot
/// be one generic impl.
macro_rules! impl_clock_step_context {
    ($($context:ident),+ $(,)?) => {
        $(
            impl<'ctx> ClockStepContext<'ctx> for $context<'ctx> {
                fn run_clock_step(
                    &self,
                    name: &'static str,
                ) -> impl Future<Output = Result<u64, TerminalError>> + Send {
                    self.run(|| async { Ok(unix_seconds()) }).name(name)
                }
            }
        )+
    };
}

impl_clock_step_context!(
    Context,
    ObjectContext,
    SharedObjectContext,
    WorkflowContext,
    SharedWorkflowContext,
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The reading is in seconds, not millis: a stamp in the wrong unit is the kind of
    /// mistake nothing downstream notices until dates read as 1970 or as the far future.
    #[test]
    fn the_clock_reads_whole_seconds_since_the_epoch() {
        let now = unix_seconds();
        // 2020-01-01 and 2100-01-01.
        assert!((1_577_836_800..4_102_444_800).contains(&now), "{now}");
    }
}
