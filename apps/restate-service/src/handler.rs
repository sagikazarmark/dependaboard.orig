//! How a handler reports how it ended: its outcome on success, its cause on failure.

use std::time::Instant;

use dependaboard_core::ActionOutcome;
use restate_sdk::prelude::*;
use thiserror::Error;
use tracing::{debug, info, warn};

/// A failure worth retrying inside a `ctx.run`. Once the step's retry budget is exhausted the
/// SDK surfaces this message as the terminal failure, so it must read well on its own.
#[derive(Debug, Error)]
pub(crate) enum RetryableServiceError {
    #[error("transient GitHub failure: {0}")]
    Github(String),
    #[error("transient projection-store failure: {0}")]
    Store(String),
    /// The finished batch's record met a failure the store does not expect to clear on
    /// its own, and the step retries regardless, since nothing later would redo it.
    #[error("batch record met a terminal-class store error; retrying regardless: {0}")]
    StoreRefusedRecord(String),
}

/// How a handler's successful result reads in its completion log line.
pub(crate) trait HandlerOutcome {
    fn outcome(&self) -> String;
}

impl HandlerOutcome for () {
    fn outcome(&self) -> String {
        "ok".to_owned()
    }
}

/// For handlers whose only interesting result is a count, e.g. how many objects they
/// fanned out to; the handler phrases it.
impl HandlerOutcome for String {
    fn outcome(&self) -> String {
        self.clone()
    }
}

impl HandlerOutcome for Json<ActionOutcome> {
    fn outcome(&self) -> String {
        match &self.0 {
            ActionOutcome::Succeeded { .. } => "succeeded".to_owned(),
            ActionOutcome::Rejected { reason } => format!("rejected: {reason}"),
        }
    }
}

impl<T> HandlerOutcome for Json<Option<T>> {
    fn outcome(&self) -> String {
        match &self.0 {
            Some(_) => "found".to_owned(),
            None => "absent".to_owned(),
        }
    }
}

/// Runs one handler attempt and logs how it ended, with the object key and elapsed time.
///
/// Called from inside the handler so the line carries the key; the SDK's span already names
/// the service and method. An attempt that suspends and replays logs once, when it really
/// finishes; an attempt that fails retryably logs each failure. `elapsed_ms` is this
/// attempt's wall time, not the invocation's age.
pub(crate) async fn traced<T: HandlerOutcome>(
    handler: &'static str,
    key: &str,
    attempt: impl Future<Output = HandlerResult<T>>,
) -> HandlerResult<T> {
    let (result, elapsed_ms) = timed(attempt).await;
    match &result {
        Ok(value) => info!(
            handler,
            key,
            outcome = value.outcome().as_str(),
            elapsed_ms,
            "handler completed"
        ),
        Err(error) => log_handler_failure(handler, key, error, elapsed_ms),
    }
    result
}

/// `traced` for the read-only handlers the dashboard polls: success is `debug`, so a busy
/// dashboard cannot drown the write path in the log.
pub(crate) async fn traced_read<T: HandlerOutcome>(
    handler: &'static str,
    key: &str,
    attempt: impl Future<Output = HandlerResult<T>>,
) -> HandlerResult<T> {
    let (result, elapsed_ms) = timed(attempt).await;
    match &result {
        Ok(value) => debug!(
            handler,
            key,
            outcome = value.outcome().as_str(),
            elapsed_ms,
            "handler completed"
        ),
        Err(error) => log_handler_failure(handler, key, error, elapsed_ms),
    }
    result
}

async fn timed<T>(attempt: impl Future<Output = T>) -> (T, u128) {
    let started = Instant::now();
    let result = attempt.await;
    (result, started.elapsed().as_millis())
}

fn log_handler_failure(handler: &'static str, key: &str, error: &HandlerError, elapsed_ms: u128) {
    warn!(
        handler,
        key,
        outcome = "failed",
        cause = %handler_cause(error),
        elapsed_ms,
        "handler failed"
    );
}

/// `HandlerError` only renders through `AsRef<dyn Error>`; its message already says whether
/// Restate will treat the failure as terminal or retry it.
pub(crate) fn handler_cause(error: &HandlerError) -> &dyn std::error::Error {
    error.as_ref()
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{PrState, RejectReason};
    use tracing::instrument::WithSubscriber;

    use super::*;
    use crate::test_support::LogSink;

    #[tokio::test]
    async fn a_completed_handler_logs_its_key_outcome_and_duration() {
        let sink = LogSink::default();

        let outcome = traced("PullRequest/merge", "7#9", async {
            Ok(Json::from(ActionOutcome::Rejected {
                reason: RejectReason::NotMergeable,
            }))
        })
        .with_subscriber(sink.subscriber())
        .await
        .unwrap();

        assert!(matches!(
            outcome.into_inner(),
            ActionOutcome::Rejected { .. }
        ));
        let logs = sink.contents();
        assert!(logs.contains("INFO"), "{logs}");
        assert!(logs.contains(r#"handler="PullRequest/merge""#), "{logs}");
        assert!(logs.contains(r#"key="7#9""#), "{logs}");
        assert!(
            logs.contains(
                r#"outcome="rejected: GitHub reports this pull request is not mergeable""#
            ),
            "{logs}"
        );
        assert!(logs.contains("elapsed_ms="), "{logs}");
    }

    #[tokio::test]
    async fn a_failed_handler_logs_the_cause_at_warn() {
        let sink = LogSink::default();

        let result = traced::<()>("WebhookIngress/dispatch", "pull_request.opened", async {
            Err(TerminalError::new("pull_request webhook is missing number").into())
        })
        .with_subscriber(sink.subscriber())
        .await;

        assert!(result.is_err());
        let logs = sink.contents();
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains(r#"key="pull_request.opened""#), "{logs}");
        assert!(logs.contains(r#"outcome="failed""#), "{logs}");
        assert!(
            logs.contains("Terminal error [500]: pull_request webhook is missing number"),
            "{logs}"
        );
    }

    #[tokio::test]
    async fn polled_reads_complete_quietly_at_debug() {
        let sink = LogSink::default();

        traced_read("PullRequest/status", "7#9", async {
            Ok(Json::from(Option::<PrState>::None))
        })
        .with_subscriber(sink.subscriber())
        .await
        .unwrap();

        let logs = sink.contents();
        assert!(logs.contains("DEBUG"), "{logs}");
        assert!(logs.contains(r#"outcome="absent""#), "{logs}");
    }
}
