//! The Restate ingress client: one-way sends, request/response calls, and the
//! paths the dashboard addresses its virtual objects by.

use std::time::Duration;

use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

use crate::server::config::RestateConfig;

/// What a request to the Restate ingress can fail with, in the classes a caller
/// consults: no answer, an answer that refused, and an answer that could not be
/// read. A refusal carries its status, since the code is what tells a workflow
/// Restate never had (404) from a refused key (401) or a handler that is
/// down (5xx), where the message alone would say only that Restate said no.
#[derive(Debug, Error)]
pub(crate) enum RestateIngressError {
    /// The request never completed: the client could not be built, the
    /// connection failed or timed out, or the response was lost before it
    /// could be read.
    #[error("Restate transport failed: {0}")]
    Transport(#[source] reqwest::Error),
    /// Restate answered with a non-success status; `body` is what it said.
    #[error("Restate returned {code}: {body}")]
    Status { code: StatusCode, body: String },
    /// A successful answer whose body could not be read as the handler's
    /// output.
    #[error("Restate returned an invalid response: {0}")]
    Decode(#[source] serde_json::Error),
}

/// The Restate ingress this deployment enqueues work on.
///
/// Built once at startup around one connection pool and shared with the
/// webhook route through its router state and with the `#[server]` functions
/// through [`crate::server::state::ServerState`].
#[derive(Clone)]
pub(crate) struct RestateIngress {
    client: reqwest::Client,
    base: String,
    token: Option<SecretString>,
}

impl RestateIngress {
    pub(crate) fn new(config: RestateConfig) -> Result<Self, RestateIngressError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(RestateIngressError::Transport)?;
        Ok(Self {
            client,
            base: config.base,
            token: config.token,
        })
    }

    /// Enqueues a one-way invocation, returning once Restate has accepted it.
    ///
    /// With an `idempotency_key`, Restate answers a repeat of the same key with
    /// the original acceptance instead of running the handler again, or, for a
    /// workflow, instead of refusing the second submission.
    pub(crate) async fn send<T: Serialize + ?Sized>(
        &self,
        path: &str,
        input: &T,
        idempotency_key: Option<&str>,
    ) -> Result<(), RestateIngressError> {
        let mut request = self.post("send", path).json(input);
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        self.accept(request).await
    }

    /// Enqueues a one-way invocation of a handler that takes no input.
    ///
    /// Restate rejects any body for such a handler, even an empty JSON one, so
    /// the request carries neither a body nor a content type.
    pub(crate) async fn send_empty(&self, path: &str) -> Result<(), RestateIngressError> {
        self.accept(self.post("send", path)).await
    }

    /// An authenticated POST to `/restate/{route}/{path}`, for `route` being
    /// Restate's `send` (one-way) or `call` (request/response).
    fn post(&self, route: &str, path: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .post(format!("{}/restate/{route}/{path}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose_secret());
        }
        request
    }

    /// Sends a one-way invocation and reads Restate's answer as accepted or not.
    async fn accept(&self, request: reqwest::RequestBuilder) -> Result<(), RestateIngressError> {
        let response = request
            .send()
            .await
            .map_err(RestateIngressError::Transport)?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(Self::refusal(response).await)
    }

    /// What Restate refused with: its status, and the body it said so in. A
    /// body that could not be read is left empty; the status is the answer.
    async fn refusal(response: reqwest::Response) -> RestateIngressError {
        let code = response.status();
        let body = response.text().await.unwrap_or_default();
        RestateIngressError::Status { code, body }
    }

    /// Makes a request/response call and reads the handler's output out of
    /// Restate's answer.
    pub(crate) async fn call<R>(&self, path: &str) -> Result<R, RestateIngressError>
    where
        R: DeserializeOwned,
    {
        let response = self
            .post("call", path)
            .send()
            .await
            .map_err(RestateIngressError::Transport)?;
        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        let body = response
            .text()
            .await
            .map_err(RestateIngressError::Transport)?;
        let value: Value = serde_json::from_str(&body).map_err(RestateIngressError::Decode)?;
        let output = value.get("output").cloned().unwrap_or(value);
        serde_json::from_value(output).map_err(RestateIngressError::Decode)
    }
}

pub(crate) fn pr_status_path(repository_id: u64, number: u64) -> String {
    format!("PullRequest/{repository_id}%23{number}/status")
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Bytes,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use dependaboard_core::{BatchProgress, PrState};

    use super::*;
    use crate::server::test_support::{closed_port, ingress_at, serve};

    #[tokio::test]
    async fn empty_input_restate_call_has_no_body_or_content_type() {
        async fn restate_ingress(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
            if headers.contains_key(header::CONTENT_TYPE) || !body.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "code": 400,
                        "message": "input validation error: Expected body and content-type to be empty, but wasn't",
                        "source": "ingress"
                    })),
                );
            }
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "output": null })),
            )
        }

        let address = serve(axum::Router::new().route(
            "/restate/call/PullRequest/7%239/status",
            post(restate_ingress),
        ))
        .await;

        let result = ingress_at(address)
            .call::<Option<PrState>>("PullRequest/7%239/status")
            .await;

        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn empty_input_restate_send_has_no_body_or_content_type() {
        async fn restate_ingress(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
            if headers.contains_key(header::CONTENT_TYPE) || !body.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "code": 400,
                        "message": "input validation error: Expected body and content-type to be empty, but wasn't",
                        "source": "ingress"
                    })),
                );
            }
            (
                StatusCode::ACCEPTED,
                axum::Json(serde_json::json!({
                    "invocationId": "inv_1",
                    "status": "Accepted"
                })),
            )
        }

        let address = serve(axum::Router::new().route(
            "/restate/send/DashboardIngress/sync_installation",
            post(restate_ingress),
        ))
        .await;

        let result = ingress_at(address)
            .send_empty("DashboardIngress/sync_installation")
            .await;

        result.unwrap();
    }

    #[test]
    fn pull_request_status_path_encodes_the_object_key_separator() {
        assert_eq!(pr_status_path(7, 9), "PullRequest/7%239/status");
    }

    /// A workflow this Restate never had answers its shared handler with a 404, and the
    /// caller must be able to tell that from every other refusal: it is what lets a
    /// progress read say "no such batch" rather than "Restate is down".
    #[tokio::test]
    async fn a_not_found_from_restate_is_a_status_error_carrying_the_code() {
        async fn restate_ingress() -> impl IntoResponse {
            (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({
                    "code": 404,
                    "message": "Not found",
                    "source": "ingress"
                })),
            )
        }

        let address = serve(axum::Router::new().route(
            "/restate/call/BulkAction/batch-1/progress",
            post(restate_ingress),
        ))
        .await;

        let error = ingress_at(address)
            .call::<Option<BatchProgress>>("BulkAction/batch-1/progress")
            .await
            .expect_err("a 404 is not an answer");

        assert!(
            matches!(
                &error,
                RestateIngressError::Status { code: StatusCode::NOT_FOUND, body }
                    if body.contains("Not found")
            ),
            "{error:?}"
        );
    }

    /// No answer at all — nothing listening where Restate should be — is its own class,
    /// so a caller can say "unavailable" for this and nothing else.
    #[tokio::test]
    async fn no_answer_from_restate_is_a_transport_error() {
        let error = ingress_at(closed_port())
            .call::<Option<BatchProgress>>("BulkAction/batch-1/progress")
            .await
            .expect_err("nobody answers on a closed port");

        assert!(
            matches!(error, RestateIngressError::Transport(_)),
            "{error:?}"
        );
    }

    /// A success whose body is not the handler's output — a proxy's maintenance page in
    /// front of Restate, say — is neither Restate refusing nor Restate away.
    #[tokio::test]
    async fn an_unreadable_answer_from_restate_is_a_decode_error() {
        async fn restate_ingress() -> impl IntoResponse {
            (StatusCode::OK, "<html>Back soon</html>")
        }

        let address = serve(axum::Router::new().route(
            "/restate/call/BulkAction/batch-1/progress",
            post(restate_ingress),
        ))
        .await;

        let error = ingress_at(address)
            .call::<Option<BatchProgress>>("BulkAction/batch-1/progress")
            .await
            .expect_err("a page is not progress");

        assert!(matches!(error, RestateIngressError::Decode(_)), "{error:?}");
    }

    /// A batch is submitted under its id as the idempotency key, so a repeat of the same
    /// submission is answered with the original acceptance. A 409 therefore means what
    /// Restate says it means, and is reported rather than read as "already accepted".
    #[tokio::test]
    async fn a_conflict_from_restate_is_an_error_even_for_a_batch() {
        async fn restate_ingress() -> impl IntoResponse {
            (
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({
                    "code": 409,
                    "message": "The invocation was previously accepted",
                    "source": "ingress"
                })),
            )
        }

        let address = serve(axum::Router::new().route(
            "/restate/send/BulkAction/batch-1/run",
            post(restate_ingress),
        ))
        .await;

        let result = ingress_at(address)
            .send(
                "BulkAction/batch-1/run",
                &serde_json::json!({}),
                Some("batch-1"),
            )
            .await;

        let error = result.expect_err("a conflict is not an acceptance");
        assert!(
            matches!(
                &error,
                RestateIngressError::Status { code: StatusCode::CONFLICT, body }
                    if body.contains("previously accepted")
            ),
            "{error:?}"
        );
    }
}
