//! Plumbing for [`restate_sdk`] services, with no application in it.
//!
//! What a durable service needs around its handlers and keeps rewriting: a named journaled
//! step with a retry budget chosen on purpose, a clock read that survives a replay, a
//! completion line per handler, a caller for the ingress, and a stop that honours SIGTERM.
//!
//! - [`step`] — journal an effect as a step: [`StepKind`] binds the retry
//!   budget to how a failure reads, so a call site chooses one thing, not two.
//! - [`clock`] — read the wall clock as a journaled step, so a replay stamps what the first
//!   run stamped.
//! - [`handler`] — log how each handler ended, with its key, outcome and elapsed time.
//! - [`telemetry`] — the subscriber that drops what a replay re-emits, which the above
//!   assumes.
//! - [`shutdown`] — a drain that starts on SIGTERM as well as SIGINT.
//! - [`bootstrap`] — ask Restate once, at startup, to arm work that then arms itself.
//! - [`config`] — read a whole-seconds setting without letting a typo pass as the default.
//! - [`ingress`] — invoke handlers from another process over Restate's ingress.
//! - [`payload`] — a handler payload that tolerates an absent body.
//! - `test_support` — fakes for all of it, behind the `test-support` feature.
//!
//! Every context trait here is implemented for all five SDK contexts, and each takes the
//! step's `name`: the name is what a replay matches a journal entry by, so two steps in one
//! invocation need two names.

pub mod bootstrap;
pub mod clock;
pub mod config;
pub mod handler;
pub mod ingress;
pub mod payload;
pub mod shutdown;
pub mod step;
pub mod telemetry;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use bootstrap::send_until_accepted;
pub use clock::{ClockStepContext, unix_seconds};
pub use config::{resolve_seconds, seconds_from_env};
pub use handler::{HandlerOutcome, RetryableStepError, handler_cause, traced, traced_read};
pub use ingress::{IngressClient, IngressConfig, IngressError};
pub use payload::Optional;
pub use shutdown::shutdown_signal;
pub use step::{ClassifiedError, FailureClass, StepContext, StepKind};

#[cfg(test)]
mod tests {
    use std::fmt;

    use restate_sdk::prelude::*;

    use super::*;
    use crate::test_support::assert_ingress_handlers;

    /// Whatever the service calls, and what it says of a failure.
    #[derive(Debug)]
    struct CalleeError;

    impl fmt::Display for CalleeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("the callee is away")
        }
    }

    impl ClassifiedError for CalleeError {
        fn class(&self) -> FailureClass {
            FailureClass::Retryable
        }
    }

    struct Smoke;

    /// A service that uses every piece a handler would: a journaled step over a classified
    /// error, a journaled clock read, an optional payload, and the completion line. It exists
    /// so the crate's pieces are held to composing inside real handler signatures — a trait
    /// whose future is not `Send` in some context, or a payload the macro will not take,
    /// fails to compile here rather than in a service that adopts the crate.
    #[restate_sdk::service]
    impl Smoke {
        #[handler]
        async fn record(&self, ctx: Context<'_>, tick: Optional<u64>) -> HandlerResult<String> {
            traced("Smoke/record", "smoke", async {
                let stamped_at = ctx.run_clock_step("stamped_at").await?;
                let recorded: u64 = ctx
                    .run_step("record", StepKind::LastChance, || async {
                        Ok::<_, CalleeError>(tick.into_inner().unwrap_or(1))
                    })
                    .await?;
                Ok(format!("recorded {recorded} at {stamped_at}"))
            })
            .await
        }

        #[handler]
        async fn status(&self, _ctx: Context<'_>) -> HandlerResult<Json<Option<u64>>> {
            traced_read("Smoke/status", "smoke", async { Ok(Json::from(None)) }).await
        }
    }

    /// The names a caller in another process addresses, as the endpoint registers them.
    #[test]
    fn the_smoke_service_registers_the_handlers_a_caller_would_address() {
        assert_ingress_handlers::<Smoke>("Smoke", &["record", "status"]);
    }
}
