//! Wraps a GitHub call as a journaled Restate step: classifies what GitHub said, waits out
//! rate limits durably, and maps the settled result onto handler results and outcomes.

use std::time::Duration;

use dependaboard_core::{
    ActionOutcome, Classification, GithubErrorResponse, Operation, RejectReason,
    classify_github_error, unix_seconds,
};
use dependaboard_github::GithubError;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::handler::RetryableServiceError;

/// How many consecutive rate-limit waits one GitHub step honours before failing terminally.
///
/// Each wait runs until the deadline GitHub advertised (up to an hour for the primary
/// limit), so this bounds a chronically over-quota installation to a few hours per step
/// instead of an invisible, unbounded loop.
const MAX_RATE_LIMIT_WAITS: u32 = 3;

/// What one attempt of a journaled GitHub step records.
///
/// The error classification is computed inside the `ctx.run` closure and journaled here, so
/// replay interprets the recorded classification instead of recomputing it. Transient
/// failures are not journaled at all: they fail the attempt so the step's bounded retry
/// policy re-runs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Attempt<T> {
    Settled(Settled<T>),
    /// GitHub asked us to back off until `until` (unix seconds). The handler sleeps durably
    /// and re-runs the step; nothing waits inside the journaled closure.
    RateLimited {
        until: u64,
        response: GithubErrorResponse,
    },
}

/// The journaled result of a GitHub step once every wait and retry has been honoured.
///
/// Failures keep the raw response next to their classification so the journal entry in
/// Restate shows what GitHub actually said, not just what we made of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Settled<T> {
    Ok(T),
    Rejected {
        reason: RejectReason,
        response: GithubErrorResponse,
    },
    Fatal {
        response: GithubErrorResponse,
    },
    StaleSha {
        expected: String,
        actual: String,
    },
}

/// Maps a GitHub call result to what the `ctx.run` closure journals.
///
/// `now` is the closure's clock reading, so the rate-limit deadline is fixed once and
/// replays verbatim. Runs inside the journaled closure; must never block.
fn journal_github_result<T>(
    result: Result<T, GithubError>,
    operation: Operation,
    known_resource: bool,
    now: u64,
) -> HandlerResult<Json<Attempt<T>>>
where
    T: Serialize + for<'de> Deserialize<'de> + 'static,
{
    let (response, known_resource) = match result {
        Ok(value) => return Ok(Json::from(Attempt::Settled(Settled::Ok(value)))),
        Err(GithubError::Http(response)) => (response, known_resource),
        Err(GithubError::HttpKnown(response)) => (response, true),
        Err(GithubError::StaleSha { expected, actual }) => {
            return Ok(Json::from(Attempt::Settled(Settled::StaleSha {
                expected,
                actual,
            })));
        }
        Err(GithubError::Transport(message)) => {
            return Err(RetryableServiceError::Github(message).into());
        }
        Err(GithubError::Protocol(message) | GithubError::Config(message)) => {
            return Err(TerminalError::new(message).into());
        }
    };
    let attempt = match classify_github_error(&response, operation, known_resource, now) {
        Classification::Retryable => {
            return Err(RetryableServiceError::Github(response.message).into());
        }
        Classification::RateLimited { until } => Attempt::RateLimited { until, response },
        Classification::Rejected(reason) => {
            Attempt::Settled(Settled::Rejected { reason, response })
        }
        Classification::Fatal => Attempt::Settled(Settled::Fatal { response }),
    };
    Ok(Json::from(attempt))
}

/// One journaled GitHub step and the durable wait a rate limit asks of Restate, abstracted
/// so `run_github_step` can be exercised against a recording fake without a runtime.
pub(crate) trait GithubStepEffects<T> {
    fn name(&self) -> &str;
    /// Runs the step once inside `ctx.run`; transient failures are retried by the step's
    /// bounded retry policy and only surface here once that budget is exhausted.
    fn attempt(&mut self) -> impl Future<Output = Result<Attempt<T>, TerminalError>> + Send;
    /// Sleeps durably until `until` (unix seconds).
    fn sleep_until(&mut self, until: u64)
    -> impl Future<Output = Result<(), TerminalError>> + Send;
}

pub(crate) struct RestateGithubStep<'a, 'ctx, F> {
    pub(crate) ctx: &'a ObjectContext<'ctx>,
    pub(crate) name: &'static str,
    pub(crate) operation: Operation,
    pub(crate) known_resource: bool,
    pub(crate) call: F,
}

impl<T, F, Fut> GithubStepEffects<T> for RestateGithubStep<'_, '_, F>
where
    T: Serialize + for<'de> Deserialize<'de> + Send + 'static,
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<T, GithubError>> + Send + 'static,
{
    fn name(&self) -> &str {
        self.name
    }

    async fn attempt(&mut self) -> Result<Attempt<T>, TerminalError> {
        let call = self.call.clone();
        let operation = self.operation;
        let known_resource = self.known_resource;
        self.ctx
            .run(move || async move {
                journal_github_result(call().await, operation, known_resource, unix_seconds())
            })
            .retry_policy(github_retry_policy())
            .name(self.name)
            .await
            .map(Json::into_inner)
    }

    async fn sleep_until(&mut self, until: u64) -> Result<(), TerminalError> {
        // The sleep entry journals its own wake-up time and replay matches it by position,
        // not by duration, so this clock reading needs no journaling of its own.
        self.ctx
            .sleep(Duration::from_secs(until.saturating_sub(unix_seconds())))
            .await
    }
}

/// Runs a GitHub call as a journaled step until it settles.
///
/// A rate-limited attempt sleeps durably until the advertised deadline and re-runs the
/// step; the wait is a journal entry, visible in Restate, and survives a process restart.
/// A step that is still rate limited after `MAX_RATE_LIMIT_WAITS` waits fails terminally
/// rather than looping out of sight.
pub(crate) async fn run_github_step<T, E: GithubStepEffects<T>>(
    step: &mut E,
) -> HandlerResult<Settled<T>> {
    let mut waits = 0;
    loop {
        let (until, response) = match step.attempt().await? {
            Attempt::Settled(settled) => return Ok(settled),
            Attempt::RateLimited { until, response } => (until, response),
        };
        if waits == MAX_RATE_LIMIT_WAITS {
            let name = step.name();
            let message = response.message;
            return Err(TerminalError::new(format!(
                "GitHub kept rate limiting {name} after {MAX_RATE_LIMIT_WAITS} waits; last reset at {until}: {message}"
            ))
            .into());
        }
        waits += 1;
        info!(
            step = step.name(),
            until,
            wait = waits,
            message = %response.message,
            "GitHub rate limited; sleeping durably until the limit resets"
        );
        step.sleep_until(until).await?;
    }
}

/// Bounded backoff for GitHub calls: transient failures (5xx, transport) back off from one
/// second to five minutes and give up after thirty minutes with a terminal failure.
fn github_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5 * 60))
        .max_duration(Duration::from_secs(30 * 60))
}

pub(crate) fn read_result<T>(result: Settled<T>) -> HandlerResult<T> {
    match result {
        Settled::Ok(value) => Ok(value),
        Settled::Rejected { reason, .. } => Err(TerminalError::new(reason.to_string()).into()),
        Settled::Fatal { response } => Err(TerminalError::new(format!(
            "GitHub read failed with HTTP {}: {}",
            response.status, response.message
        ))
        .into()),
        Settled::StaleSha { .. } => {
            Err(TerminalError::new("unexpected stale SHA while reading GitHub").into())
        }
    }
}

pub(crate) fn action_result(result: Settled<String>) -> HandlerResult<ActionOutcome> {
    match result {
        Settled::Ok(detail) => Ok(ActionOutcome::Succeeded { detail }),
        Settled::StaleSha { expected, actual } => {
            Ok(rejected(RejectReason::StaleSha { expected, actual }))
        }
        Settled::Rejected { reason, .. } => Ok(rejected(reason)),
        Settled::Fatal { response } => Err(TerminalError::new(format!(
            "GitHub mutation failed with HTTP {}: {}",
            response.status, response.message
        ))
        .into()),
    }
}

pub(crate) fn rejected(reason: RejectReason) -> ActionOutcome {
    ActionOutcome::Rejected { reason }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::store::store_retry_policy;

    fn is_retryable(error: &HandlerError) -> bool {
        let cause: &dyn std::error::Error = error.as_ref();
        cause.to_string().starts_with("Retryable error")
    }

    fn is_terminal(error: &HandlerError) -> bool {
        let cause: &dyn std::error::Error = error.as_ref();
        cause.to_string().starts_with("Terminal error")
    }

    /// Journals a failed GitHub call at `now = 1_000` and returns the attempt's failure.
    fn journal_failure(error: GithubError, operation: Operation) -> HandlerError {
        match journal_github_result::<()>(Err(error), operation, false, 1_000) {
            Err(error) => error,
            Ok(attempt) => panic!(
                "expected the attempt to fail, got {:?}",
                attempt.into_inner()
            ),
        }
    }

    #[test]
    fn a_rate_limited_response_is_journaled_with_its_deadline() {
        let response = GithubErrorResponse {
            status: 403,
            message: "You have exceeded a secondary rate limit.".to_owned(),
            retry_after_seconds: Some(45),
            ..Default::default()
        };

        let attempt = journal_github_result::<()>(
            Err(GithubError::Http(response.clone())),
            Operation::Comment,
            false,
            1_000,
        )
        .expect("a rate limit is a journaled value, not a run failure")
        .into_inner();

        assert!(matches!(
            attempt,
            Attempt::RateLimited { until: 1_045, response: journaled } if journaled == response
        ));
    }

    #[test]
    fn a_transient_failure_fails_the_attempt_so_the_bounded_policy_retries_it() {
        let unavailable = GithubErrorResponse {
            status: 503,
            message: "unavailable".to_owned(),
            ..Default::default()
        };
        let error = journal_failure(GithubError::Http(unavailable), Operation::Read);
        assert!(is_retryable(&error), "{error:?}");

        let error = journal_failure(
            GithubError::Transport("connection reset".to_owned()),
            Operation::Read,
        );
        assert!(is_retryable(&error), "{error:?}");
    }

    #[test]
    fn a_rejected_response_is_journaled_with_its_reason() {
        let response = GithubErrorResponse {
            status: 404,
            message: "Not Found".to_owned(),
            ..Default::default()
        };

        let attempt = journal_github_result::<()>(
            Err(GithubError::HttpKnown(response.clone())),
            Operation::Read,
            false,
            1_000,
        )
        .unwrap()
        .into_inner();

        assert_eq!(
            attempt,
            Attempt::Settled(Settled::Rejected {
                reason: RejectReason::NotFound,
                response,
            })
        );
    }

    #[test]
    fn a_fatal_response_is_journaled_verbatim() {
        let response = GithubErrorResponse {
            status: 401,
            message: "Bad credentials".to_owned(),
            ..Default::default()
        };
        let attempt = journal_github_result::<()>(
            Err(GithubError::Http(response.clone())),
            Operation::Read,
            false,
            1_000,
        )
        .unwrap()
        .into_inner();
        assert_eq!(attempt, Attempt::Settled(Settled::Fatal { response }));
    }

    #[test]
    fn a_protocol_error_fails_the_attempt_terminally() {
        let error = journal_failure(
            GithubError::Protocol("invalid GitHub response".to_owned()),
            Operation::Read,
        );
        assert!(is_terminal(&error), "{error:?}");
    }

    /// Stands in for one journaled GitHub step and records the durable waits it asked of
    /// Restate.
    struct RecordedGithubStep {
        attempts: VecDeque<Attempt<String>>,
        slept_until: Vec<u64>,
    }

    impl RecordedGithubStep {
        fn new(attempts: impl IntoIterator<Item = Attempt<String>>) -> Self {
            Self {
                attempts: attempts.into_iter().collect(),
                slept_until: Vec::new(),
            }
        }
    }

    fn rate_limited(until: u64) -> Attempt<String> {
        Attempt::RateLimited {
            until,
            response: GithubErrorResponse {
                status: 403,
                message: "API rate limit exceeded".to_owned(),
                rate_limit_remaining: Some(0),
                rate_limit_reset: Some(until),
                ..Default::default()
            },
        }
    }

    impl GithubStepEffects<String> for RecordedGithubStep {
        fn name(&self) -> &str {
            "merge-pull-request"
        }

        async fn attempt(&mut self) -> Result<Attempt<String>, TerminalError> {
            Ok(self
                .attempts
                .pop_front()
                .expect("the step ran more attempts than the test scripted"))
        }

        async fn sleep_until(&mut self, until: u64) -> Result<(), TerminalError> {
            self.slept_until.push(until);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_rate_limited_attempt_sleeps_durably_until_the_deadline_then_retries() {
        let mut step = RecordedGithubStep::new([
            rate_limited(4_600),
            Attempt::Settled(Settled::Ok("merged".to_owned())),
        ]);

        let settled = run_github_step(&mut step).await.unwrap();

        assert_eq!(settled, Settled::Ok("merged".to_owned()));
        assert_eq!(step.slept_until, vec![4_600]);
        assert!(
            step.attempts.is_empty(),
            "the step was re-run after the wait"
        );
    }

    #[tokio::test]
    async fn a_step_that_stays_rate_limited_gives_up_terminally_after_three_waits() {
        let mut step = RecordedGithubStep::new([
            rate_limited(4_600),
            rate_limited(8_200),
            rate_limited(11_800),
            rate_limited(15_400),
        ]);

        let error = run_github_step(&mut step).await.unwrap_err();

        assert_eq!(step.slept_until, vec![4_600, 8_200, 11_800]);
        let cause: &dyn std::error::Error = error.as_ref();
        assert_eq!(
            cause.to_string(),
            "Terminal error [500]: GitHub kept rate limiting merge-pull-request after 3 waits; last reset at 15400: API rate limit exceeded"
        );
    }

    #[tokio::test]
    async fn a_settled_attempt_never_waits() {
        let mut step = RecordedGithubStep::new([Attempt::Settled(Settled::Rejected {
            reason: RejectReason::NotMergeable,
            response: GithubErrorResponse {
                status: 405,
                message: "Pull Request is not mergeable".to_owned(),
                ..Default::default()
            },
        })]);

        let settled = run_github_step(&mut step).await.unwrap();

        assert!(matches!(
            settled,
            Settled::Rejected {
                reason: RejectReason::NotMergeable,
                ..
            }
        ));
        assert!(step.slept_until.is_empty());
    }

    #[test]
    fn every_run_retry_policy_gives_up_after_a_bounded_duration() {
        // The SDK exposes no accessor for the policy's bounds, so inspect its Debug form.
        let github = format!("{:?}", github_retry_policy());
        assert!(github.contains("max_duration: Some(1800s)"), "{github}");
        let store = format!("{:?}", store_retry_policy());
        assert!(store.contains("max_duration: Some(300s)"), "{store}");
    }

    fn forbidden() -> GithubErrorResponse {
        GithubErrorResponse {
            status: 403,
            message: "Resource not accessible by integration".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn settled_mutations_map_to_outcomes_and_only_fatal_ones_fail() {
        assert_eq!(
            action_result(Settled::Rejected {
                reason: RejectReason::Forbidden,
                response: forbidden(),
            })
            .unwrap(),
            ActionOutcome::Rejected {
                reason: RejectReason::Forbidden
            }
        );
        assert_eq!(
            action_result(Settled::Ok("merged".to_owned())).unwrap(),
            ActionOutcome::Succeeded {
                detail: "merged".to_owned()
            }
        );

        let error = action_result(Settled::Fatal {
            response: GithubErrorResponse {
                status: 401,
                message: "Bad credentials".to_owned(),
                ..Default::default()
            },
        })
        .unwrap_err();
        let cause: &dyn std::error::Error = error.as_ref();
        assert_eq!(
            cause.to_string(),
            "Terminal error [500]: GitHub mutation failed with HTTP 401: Bad credentials"
        );
    }

    #[test]
    fn a_rejected_read_has_no_outcome_to_carry_it_and_fails_terminally() {
        let error = read_result(Settled::<()>::Rejected {
            reason: RejectReason::Forbidden,
            response: forbidden(),
        })
        .unwrap_err();
        assert!(is_terminal(&error), "{error:?}");
    }
}
