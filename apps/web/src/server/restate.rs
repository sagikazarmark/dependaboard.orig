//! The Restate ingress client: one method per handler this deployment invokes,
//! over one-way sends and request/response calls.
//!
//! The path each handler is addressed at — the service, the key where there is
//! one, the handler, and the idempotency key the invocation carries — is this
//! module's business and nobody else's; a caller asks for the thing it wants
//! and is answered in the handler's own terms. The names come from
//! [`dependaboard_core::restate`], which is what the Restate service's
//! discovery tests hold its registered handlers to, so the two ends of every
//! path are the same constant.

use std::time::Duration;

use dependaboard_core::{
    BatchProgress, BulkRequest, Capabilities, ManualSyncRequest, PrKey, PrState, WebhookEvent,
    restate::{
        BULK_ACTION, BULK_ACTION_PROGRESS, BULK_ACTION_RUN, DASHBOARD_CAPABILITIES,
        DASHBOARD_INGRESS, DASHBOARD_SYNC_INSTALLATION, DASHBOARD_SYNC_PULL_REQUEST, PULL_REQUEST,
        PULL_REQUEST_STATUS, WEBHOOK_DISPATCH, WEBHOOK_INGRESS,
    },
};
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

    /// What this deployment can do, as the Restate service resolved it at
    /// startup: which actions the dashboard should offer.
    pub(crate) async fn capabilities(&self) -> Result<Capabilities, RestateIngressError> {
        self.call(&format!("{DASHBOARD_INGRESS}/{DASHBOARD_CAPABILITIES}"))
            .await
    }

    /// Submits `request` as the batch `batch_id`, returning once Restate has
    /// accepted it.
    ///
    /// The batch id is the workflow key, which lets the batch run once, and the
    /// idempotency key, which lets the browser resend the same submission after
    /// a lost response and be told it was accepted rather than refused for the
    /// workflow already existing.
    pub(crate) async fn run_batch(
        &self,
        batch_id: &str,
        request: &BulkRequest,
    ) -> Result<(), RestateIngressError> {
        self.send(
            &format!("{BULK_ACTION}/{batch_id}/{BULK_ACTION_RUN}"),
            request,
            Some(batch_id),
        )
        .await
    }

    /// Where the batch stands, as Restate holds it, or `None` for a batch this
    /// Restate has never had.
    ///
    /// Restate answers the shared handler of a workflow it never had with a
    /// 404, and that is the `None`: the one refusal that says something about
    /// the batch rather than about Restate, and the one a follow gives up on.
    /// Every other failure is Restate not answering for the batch and is
    /// reported as such, so a follow waits it out. Reading the 404 here rather
    /// than at the caller is what keeps the distinction in one place — at the
    /// cost that a path this module got wrong would be answered `None` too,
    /// which is why every path here is held to a Restate serving exactly the
    /// handler it addresses (`mod tests::paths`).
    pub(crate) async fn batch_progress(
        &self,
        batch_id: &str,
    ) -> Result<Option<BatchProgress>, RestateIngressError> {
        match self
            .call(&format!("{BULK_ACTION}/{batch_id}/{BULK_ACTION_PROGRESS}"))
            .await
        {
            Ok(progress) => Ok(progress),
            Err(RestateIngressError::Status {
                code: StatusCode::NOT_FOUND,
                ..
            }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The durable state Restate holds for the pull request `key`, or `None`
    /// for one its object holds nothing for.
    pub(crate) async fn pull_request_status(
        &self,
        key: &PrKey,
    ) -> Result<Option<PrState>, RestateIngressError> {
        self.call(&pr_status_path(key.repository_id, key.number))
            .await
    }

    /// Asks Restate to reconcile the whole installation now, without waiting
    /// for the scheduler's next sweep.
    pub(crate) async fn sync_installation(&self) -> Result<(), RestateIngressError> {
        self.send_empty(&format!(
            "{DASHBOARD_INGRESS}/{DASHBOARD_SYNC_INSTALLATION}"
        ))
        .await
    }

    /// Asks Restate to refresh one pull request now. The request names the
    /// completion id the refresh will be recorded under, so no idempotency key
    /// is wanted: two refreshes asked for are two refreshes.
    pub(crate) async fn sync_pull_request(
        &self,
        request: &ManualSyncRequest,
    ) -> Result<(), RestateIngressError> {
        self.send(
            &format!("{DASHBOARD_INGRESS}/{DASHBOARD_SYNC_PULL_REQUEST}"),
            request,
            None,
        )
        .await
    }

    /// Forwards a verified GitHub delivery to the dispatcher.
    ///
    /// A redelivery from the App's delivery log replays the request under the
    /// same delivery id, so it is the key that lets Restate fold the replay
    /// into the dispatch it already accepted.
    pub(crate) async fn dispatch_webhook(
        &self,
        event: &WebhookEvent,
        delivery_id: &str,
    ) -> Result<(), RestateIngressError> {
        self.send(
            &format!("{WEBHOOK_INGRESS}/{WEBHOOK_DISPATCH}"),
            event,
            Some(delivery_id),
        )
        .await
    }

    /// Enqueues a one-way invocation, returning once Restate has accepted it.
    ///
    /// With an `idempotency_key`, Restate answers a repeat of the same key with
    /// the original acceptance instead of running the handler again, or, for a
    /// workflow, instead of refusing the second submission.
    async fn send<T: Serialize + ?Sized>(
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
    async fn send_empty(&self, path: &str) -> Result<(), RestateIngressError> {
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
    async fn call<R>(&self, path: &str) -> Result<R, RestateIngressError>
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

/// The path the `PullRequest` object for `repository_id` and `number` is
/// addressed at. The object key is `{repository_id}#{number}`, and the `#` has
/// to reach Restate percent-encoded or the path stops at the repository id.
///
/// Named apart from [`RestateIngress::pull_request_status`] so it can be
/// asserted without a socket, and so a test that stands in for Restate can put
/// state at the path the drawer's read will ask for.
pub(crate) fn pr_status_path(repository_id: u64, number: u64) -> String {
    format!("{PULL_REQUEST}/{repository_id}%23{number}/{PULL_REQUEST_STATUS}")
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::{
        body::Bytes,
        http::{HeaderMap, StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use dependaboard_core::{
        BatchProgress, BulkActionKind, BulkRequest, Capabilities, ManualSyncRequest, PrKey,
        PrState, PrTarget, UserId, WebhookEvent,
    };

    use super::*;
    use crate::server::test_support::{closed_port, ingress_at, serve};

    /// Each named method is tested against a Restate that has exactly one handler: the
    /// one it is supposed to address. Anything else is a 404 from the router, so the
    /// assertion that the call *succeeded* is the assertion that the path was right.
    ///
    /// That matters most for [`RestateIngress::batch_progress`], which reads a 404 as
    /// "no such batch" and answers `None`: a mistyped path there is not an error but a
    /// silent, plausible answer. Its test therefore asserts the progress it put behind
    /// the one route comes back — `Some`, never `None`.
    mod paths {
        use super::*;

        /// What Restate answers an accepted one-way send with.
        fn accepted() -> impl IntoResponse {
            (
                StatusCode::ACCEPTED,
                axum::Json(serde_json::json!({
                    "invocationId": "inv_1",
                    "status": "Accepted"
                })),
            )
        }

        /// What Restate answers a call with: the handler's output, wrapped as its
        /// ingress wraps it.
        fn output(value: serde_json::Value) -> impl IntoResponse {
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "output": value })),
            )
        }

        /// A Restate whose ingress has exactly one route, at `path`.
        async fn restate_serving(path: &str, handler: axum::routing::MethodRouter) -> SocketAddr {
            serve(axum::Router::new().route(path, handler)).await
        }

        /// The batch every batch test here submits and follows.
        const BATCH_ID: &str = "0199c0ff-ee00-7000-8000-000000000001";

        fn bulk_request() -> BulkRequest {
            BulkRequest {
                action: BulkActionKind::Merge,
                targets: vec![PrTarget {
                    repository_id: 7,
                    owner: "acme".to_owned(),
                    repo: "api".to_owned(),
                    number: 9,
                    expected_sha: "abc123".to_owned(),
                    title: "Bump serde".to_owned(),
                    html_url: "https://github.com/acme/api/pull/9".to_owned(),
                }],
                user_id: UserId::new("dependaboard"),
                retried_from: None,
            }
        }

        #[tokio::test]
        async fn capabilities_are_read_from_the_dashboard_ingress_service() {
            let address = restate_serving(
                "/restate/call/DashboardIngress/capabilities",
                post(|| async { output(serde_json::json!({ "rebase_enabled": true })) }),
            )
            .await;

            assert_eq!(
                ingress_at(address).capabilities().await.unwrap(),
                Capabilities {
                    rebase_enabled: true
                }
            );
        }

        /// The batch id is both the workflow key in the path and the idempotency key, so
        /// this Restate refuses a submission that arrives without it.
        #[tokio::test]
        async fn a_batch_is_submitted_to_its_workflow_under_its_id() {
            let address = restate_serving(
                &format!("/restate/send/BulkAction/{BATCH_ID}/run"),
                post(|headers: HeaderMap| async move {
                    if headers
                        .get("idempotency-key")
                        .and_then(|key| key.to_str().ok())
                        != Some(BATCH_ID)
                    {
                        return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({})))
                            .into_response();
                    }
                    accepted().into_response()
                }),
            )
            .await;

            ingress_at(address)
                .run_batch(BATCH_ID, &bulk_request())
                .await
                .expect("the batch is submitted to the one handler this Restate has");
        }

        /// The one whose path a wrong answer would hide: a 404 here is `None`, so this
        /// asserts the progress behind the route comes back rather than that the call
        /// did not fail.
        #[tokio::test]
        async fn a_batch_is_followed_at_its_workflows_shared_progress_handler() {
            let progress = BatchProgress::queued(BATCH_ID, BulkActionKind::Merge, &[]);
            let answer = serde_json::to_value(&progress).unwrap();
            let address = restate_serving(
                &format!("/restate/call/BulkAction/{BATCH_ID}/progress"),
                post(move || {
                    let answer = answer.clone();
                    async move { output(answer) }
                }),
            )
            .await;

            assert_eq!(
                ingress_at(address).batch_progress(BATCH_ID).await.unwrap(),
                Some(progress),
                "the progress behind the one route this Restate has"
            );
        }

        /// The canonical one: the object key is `{repository_id}#{number}`, and the `#`
        /// has to reach Restate percent-encoded or the path stops at the repository id.
        #[tokio::test]
        async fn a_pull_requests_state_is_read_at_the_encoded_object_key() {
            let mut state = PrState::default();
            state.complete_sync("sync-1".to_owned());
            let answer = serde_json::to_value(&state).unwrap();
            let address = restate_serving(
                "/restate/call/PullRequest/7%239/status",
                post(move || {
                    let answer = answer.clone();
                    async move { output(answer) }
                }),
            )
            .await;

            assert_eq!(
                ingress_at(address)
                    .pull_request_status(&PrKey::new(7, 9))
                    .await
                    .unwrap(),
                Some(state),
                "the state behind the one route this Restate has"
            );
        }

        #[tokio::test]
        async fn an_installation_sync_is_asked_of_the_dashboard_ingress_service() {
            let address = restate_serving(
                "/restate/send/DashboardIngress/sync_installation",
                post(|| async { accepted() }),
            )
            .await;

            ingress_at(address)
                .sync_installation()
                .await
                .expect("the sync is sent to the one handler this Restate has");
        }

        #[tokio::test]
        async fn a_pull_request_sync_is_asked_of_the_dashboard_ingress_service() {
            let address = restate_serving(
                "/restate/send/DashboardIngress/sync_pull_request",
                post(|| async { accepted() }),
            )
            .await;

            ingress_at(address)
                .sync_pull_request(&ManualSyncRequest {
                    repository_id: 7,
                    owner: "acme".to_owned(),
                    repo: "api".to_owned(),
                    number: 9,
                    completion_id: "sync-1".to_owned(),
                })
                .await
                .expect("the sync is sent to the one handler this Restate has");
        }

        /// The delivery id is the idempotency key, so this Restate refuses a forward
        /// that arrives without it: a redelivery must be foldable into the dispatch it
        /// already accepted.
        #[tokio::test]
        async fn a_webhook_delivery_is_forwarded_to_the_dispatcher_under_its_delivery_id() {
            const DELIVERY_ID: &str = "72d3162e-cc78-11e3-81ab-4c9367dc0958";

            let address = restate_serving(
                "/restate/send/WebhookIngress/dispatch",
                post(|headers: HeaderMap| async move {
                    if headers
                        .get("idempotency-key")
                        .and_then(|key| key.to_str().ok())
                        != Some(DELIVERY_ID)
                    {
                        return (StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({})))
                            .into_response();
                    }
                    accepted().into_response()
                }),
            )
            .await;

            ingress_at(address)
                .dispatch_webhook(
                    &WebhookEvent {
                        event: "pull_request".to_owned(),
                        action: Some("synchronize".to_owned()),
                        installation_id: Some(42),
                        repository_id: Some(7),
                        owner: Some("acme".to_owned()),
                        repo: Some("api".to_owned()),
                        number: Some(9),
                        sha: Some("abc123".to_owned()),
                        pull_requests: Vec::new(),
                    },
                    DELIVERY_ID,
                )
                .await
                .expect("the delivery is forwarded to the one handler this Restate has");
        }
    }

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
