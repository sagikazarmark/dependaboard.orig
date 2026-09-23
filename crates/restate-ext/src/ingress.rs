//! A client for Restate's ingress: one-way sends and request/response calls over HTTP.
//!
//! The Rust SDK writes handlers, not callers, so a process outside the endpoint — a web
//! edge, a CLI, another service — reaches Restate over the ingress by hand. This is that
//! by hand, once: the URL shape, the idempotency header, the answer envelope and the
//! classes a failure comes back in.
//!
//! What it deliberately does not know is which handlers exist. A deployment wraps it in a
//! type with one method per handler it invokes, so a caller asks for the thing it wants and
//! the path stays in one place; [`handler_path`] and [`keyed_handler_path`] build the paths
//! those methods pass.

use std::{fmt::Write as _, time::Duration};

use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

/// What a request to the Restate ingress can fail with, in the classes a caller consults:
/// no answer, an answer that refused, and an answer that could not be read. A refusal
/// carries its status, since the code is what tells an invocation Restate never had (404)
/// from a refused key (401) or a handler that is down (5xx), where the message alone would
/// say only that Restate said no.
#[derive(Debug, Error)]
pub enum IngressError {
    /// The request never completed: the client could not be built, the connection failed or
    /// timed out, or the response was lost before it could be read.
    #[error("Restate transport failed: {0}")]
    Transport(#[source] reqwest::Error),
    /// Restate answered with a non-success status; `body` is what it said.
    #[error("Restate returned {code}: {body}")]
    Status { code: StatusCode, body: String },
    /// A successful answer whose body could not be read as the handler's output.
    #[error("Restate returned an invalid response: {0}")]
    Decode(#[source] serde_json::Error),
}

/// Where the ingress is and how to talk to it.
#[derive(Clone, Debug)]
pub struct IngressConfig {
    /// The ingress root, without a trailing slash; [`Self::new`] trims one.
    pub base: String,
    /// The bearer token, for an ingress that requires one.
    pub token: Option<SecretString>,
    pub connect_timeout: Duration,
    /// The whole-request budget. It bounds a request/response call to a handler that runs
    /// arbitrarily long, so a caller that invokes one should either raise it or send the
    /// invocation one-way and poll.
    pub timeout: Duration,
}

impl IngressConfig {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
            token: None,
            connect_timeout: Duration::from_secs(5),
            timeout: Duration::from_secs(15),
        }
    }

    pub fn with_token(mut self, token: impl Into<SecretString>) -> Self {
        self.token = Some(token.into());
        self
    }
}

/// The Restate ingress this process enqueues work on.
///
/// Built once at startup around one connection pool and shared; cloning shares it.
#[derive(Clone)]
pub struct IngressClient {
    client: reqwest::Client,
    base: String,
    token: Option<SecretString>,
}

impl IngressClient {
    pub fn new(config: IngressConfig) -> Result<Self, IngressError> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .build()
            .map_err(IngressError::Transport)?;
        Ok(Self {
            client,
            base: config.base,
            token: config.token,
        })
    }

    /// The ingress root this client posts to, without a trailing slash.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Enqueues a one-way invocation, returning once Restate has accepted it.
    ///
    /// With an `idempotency_key`, Restate answers a repeat of the same key with the original
    /// acceptance instead of running the handler again, or, for a workflow, instead of
    /// refusing the second submission. A delivery id, a submission id — anything the caller
    /// would resend the same request under — belongs here.
    pub async fn send<T: Serialize + ?Sized>(
        &self,
        path: &str,
        input: &T,
        idempotency_key: Option<&str>,
    ) -> Result<(), IngressError> {
        let mut request = self.post("send", path).json(input);
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        self.accept(request).await
    }

    /// Enqueues a one-way invocation of a handler that takes no input.
    ///
    /// Restate rejects any body for such a handler, even an empty JSON one, so the request
    /// carries neither a body nor a content type. A handler that should accept both an
    /// argument and nothing takes [`Optional`](crate::payload::Optional) instead.
    pub async fn send_empty(&self, path: &str) -> Result<(), IngressError> {
        self.accept(self.post("send", path)).await
    }

    /// Makes a request/response call to a handler that takes no input and reads its output.
    pub async fn call<R: DeserializeOwned>(&self, path: &str) -> Result<R, IngressError> {
        self.read_output(self.post("call", path)).await
    }

    /// Makes a request/response call with `input` and reads the handler's output.
    pub async fn call_with<T: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        path: &str,
        input: &T,
    ) -> Result<R, IngressError> {
        self.read_output(self.post("call", path).json(input)).await
    }

    /// [`Self::call`], reading a 404 as `None`.
    ///
    /// Restate answers the shared handler of a workflow it never had with a 404, and that is
    /// the `None`: the one refusal that says something about the invocation rather than
    /// about Restate, and the one a follow gives up on. Every other failure is Restate not
    /// answering and is reported as such, so a follow waits it out.
    ///
    /// The cost is that a path the caller got wrong is answered `None` too — not an error
    /// but a silent, plausible answer. That is what `test_support::assert_ingress_handlers`
    /// and a test against a Restate serving exactly the one handler are for.
    pub async fn call_optional<R: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Option<R>, IngressError> {
        match self.call(path).await {
            Ok(output) => Ok(Some(output)),
            Err(IngressError::Status {
                code: StatusCode::NOT_FOUND,
                ..
            }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// An authenticated POST to `/restate/{route}/{path}`, for `route` being Restate's
    /// `send` (one-way) or `call` (request/response).
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
    async fn accept(&self, request: reqwest::RequestBuilder) -> Result<(), IngressError> {
        let response = request.send().await.map_err(IngressError::Transport)?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(Self::refusal(response).await)
    }

    /// What Restate refused with: its status, and the body it said so in. A body that could
    /// not be read is left empty; the status is the answer.
    async fn refusal(response: reqwest::Response) -> IngressError {
        let code = response.status();
        let body = response.text().await.unwrap_or_default();
        IngressError::Status { code, body }
    }

    /// Reads the handler's output out of Restate's answer.
    ///
    /// The ingress wraps a handler's return value as `{"output": …}`; a body that is not
    /// wrapped is read as the output itself, so a caller is not held to which shape the
    /// answer came in.
    async fn read_output<R: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<R, IngressError> {
        let response = request.send().await.map_err(IngressError::Transport)?;
        if !response.status().is_success() {
            return Err(Self::refusal(response).await);
        }
        let body = response.text().await.map_err(IngressError::Transport)?;
        let value: Value = serde_json::from_str(&body).map_err(IngressError::Decode)?;
        let output = value.get("output").cloned().unwrap_or(value);
        serde_json::from_value(output).map_err(IngressError::Decode)
    }
}

/// The path a service's handler is addressed at.
pub fn handler_path(service: &str, handler: &str) -> String {
    format!("{service}/{handler}")
}

/// The path a keyed handler — a virtual object's or a workflow's — is addressed at, with
/// `key` encoded by [`encode_object_key`].
pub fn keyed_handler_path(service: &str, key: &str, handler: &str) -> String {
    format!("{service}/{}/{handler}", encode_object_key(key))
}

/// Percent-encodes an object or workflow key for a URL path segment.
///
/// A key is arbitrary text and Restate takes it as one path segment, so anything outside
/// the unreserved set is encoded. The canonical trap is a composite key: `7#9` must reach
/// Restate as `7%239`, or the path stops at the `#` and addresses the object `7`. A `/`
/// would be worse, silently addressing a different handler.
pub fn encode_object_key(key: &str) -> String {
    let mut encoded = String::with_capacity(key.len());
    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                // Writing to a String cannot fail.
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Bytes,
        http::{HeaderMap, header},
        response::IntoResponse,
        routing::post,
    };
    use serde_json::json;

    use super::*;
    use crate::test_support::{closed_port, ingress_at, serve};

    /// What Restate answers an accepted one-way send with.
    fn accepted() -> impl IntoResponse {
        (
            StatusCode::ACCEPTED,
            axum::Json(json!({ "invocationId": "inv_1", "status": "Accepted" })),
        )
    }

    /// What Restate answers a call with: the handler's output, wrapped as its ingress wraps
    /// it.
    fn output(value: Value) -> impl IntoResponse {
        (StatusCode::OK, axum::Json(json!({ "output": value })))
    }

    /// A Restate whose ingress has exactly one route, at `path`. Anything else is a 404
    /// from the router, so a test that asserts its call *succeeded* asserts the path.
    async fn restate_serving(
        path: &str,
        handler: axum::routing::MethodRouter,
    ) -> std::net::SocketAddr {
        serve(axum::Router::new().route(path, handler)).await
    }

    #[tokio::test]
    async fn a_send_is_posted_to_the_handlers_send_path() {
        let address = restate_serving(
            "/restate/send/Greeter/greet",
            post(|body: Bytes| async move {
                if body.as_ref() != br#""world""# {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                accepted().into_response()
            }),
        )
        .await;

        ingress_at(address)
            .send("Greeter/greet", "world", None)
            .await
            .expect("the send reaches the one handler this Restate has");
    }

    /// The header is what lets a caller resend a submission after a lost response and be
    /// told it was accepted rather than refused for the invocation already existing, so
    /// this Restate refuses a send that arrives without it.
    #[tokio::test]
    async fn an_idempotency_key_is_sent_as_the_header_restate_folds_repeats_by() {
        let address = restate_serving(
            "/restate/send/Batch/batch-1/run",
            post(|headers: HeaderMap| async move {
                if headers
                    .get("idempotency-key")
                    .and_then(|key| key.to_str().ok())
                    != Some("batch-1")
                {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                accepted().into_response()
            }),
        )
        .await;

        ingress_at(address)
            .send("Batch/batch-1/run", &json!({}), Some("batch-1"))
            .await
            .expect("the send carries its idempotency key");
    }

    #[tokio::test]
    async fn a_call_reads_the_output_restate_wraps_the_answer_in() {
        let address = restate_serving(
            "/restate/call/Greeter/greet",
            post(|| async { output(json!({ "greeting": "hello" })) }),
        )
        .await;

        let answer: Value = ingress_at(address).call("Greeter/greet").await.unwrap();
        assert_eq!(answer, json!({ "greeting": "hello" }));
    }

    /// An answer that is the output itself rather than Restate's envelope — a stand-in in
    /// a test, a proxy that unwraps — is read as the output.
    #[tokio::test]
    async fn a_call_reads_an_unwrapped_answer_as_the_output() {
        let address = restate_serving(
            "/restate/call/Greeter/greet",
            post(|| async { (StatusCode::OK, axum::Json(json!({ "greeting": "hello" }))) }),
        )
        .await;

        let answer: Value = ingress_at(address).call("Greeter/greet").await.unwrap();
        assert_eq!(answer, json!({ "greeting": "hello" }));
    }

    #[tokio::test]
    async fn a_call_with_input_posts_it_and_reads_the_output() {
        let address = restate_serving(
            "/restate/call/Greeter/greet",
            post(|body: Bytes| async move {
                if body.as_ref() != br#"{"name":"world"}"# {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                output(json!("hello world")).into_response()
            }),
        )
        .await;

        let answer: String = ingress_at(address)
            .call_with("Greeter/greet", &json!({ "name": "world" }))
            .await
            .unwrap();
        assert_eq!(answer, "hello world");
    }

    /// An invocation Restate never had is `None`, not a failure: the one refusal that says
    /// something about the invocation rather than about Restate.
    #[tokio::test]
    async fn an_optional_call_reads_a_not_found_as_nothing() {
        let address = restate_serving(
            "/restate/call/Batch/batch-1/progress",
            post(|| async {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({ "code": 404, "message": "Not found", "source": "ingress" })),
                )
            }),
        )
        .await;

        let answer: Option<Value> = ingress_at(address)
            .call_optional("Batch/batch-1/progress")
            .await
            .unwrap();
        assert_eq!(answer, None);
    }

    /// The other half of the same contract: a 404 is the only refusal read as nothing, so
    /// an ingress that refuses the key is reported rather than mistaken for an invocation
    /// that does not exist.
    #[tokio::test]
    async fn an_optional_call_reports_every_other_refusal() {
        let address = restate_serving(
            "/restate/call/Batch/batch-1/progress",
            post(|| async { (StatusCode::UNAUTHORIZED, "no") }),
        )
        .await;

        let error = ingress_at(address)
            .call_optional::<Value>("Batch/batch-1/progress")
            .await
            .expect_err("a refused key is not an absent invocation");

        assert!(
            matches!(
                error,
                IngressError::Status {
                    code: StatusCode::UNAUTHORIZED,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_call_to_a_handler_with_no_input_has_no_body_or_content_type() {
        async fn restate_ingress(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
            if headers.contains_key(header::CONTENT_TYPE) || !body.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "code": 400,
                        "message": "input validation error: Expected body and content-type to be empty, but wasn't",
                        "source": "ingress"
                    })),
                );
            }
            (StatusCode::OK, axum::Json(json!({ "output": null })))
        }

        let address =
            restate_serving("/restate/call/Object/7%239/status", post(restate_ingress)).await;

        let answer = ingress_at(address)
            .call::<Option<u64>>("Object/7%239/status")
            .await
            .unwrap();
        assert_eq!(answer, None);
    }

    #[tokio::test]
    async fn a_send_to_a_handler_with_no_input_has_no_body_or_content_type() {
        async fn restate_ingress(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
            if headers.contains_key(header::CONTENT_TYPE) || !body.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "code": 400,
                        "message": "input validation error: Expected body and content-type to be empty, but wasn't",
                        "source": "ingress"
                    })),
                );
            }
            (
                StatusCode::ACCEPTED,
                axum::Json(json!({ "invocationId": "inv_1", "status": "Accepted" })),
            )
        }

        let address = restate_serving("/restate/send/Scheduler/start", post(restate_ingress)).await;

        ingress_at(address)
            .send_empty("Scheduler/start")
            .await
            .expect("a handler with no input takes no body");
    }

    /// No answer at all — nothing listening where Restate should be — is its own class, so
    /// a caller can say "unavailable" for this and nothing else.
    #[tokio::test]
    async fn no_answer_from_restate_is_a_transport_error() {
        let error = ingress_at(closed_port())
            .call::<Value>("Batch/batch-1/progress")
            .await
            .expect_err("nobody answers on a closed port");

        assert!(matches!(error, IngressError::Transport(_)), "{error:?}");
    }

    /// A success whose body is not the handler's output — a proxy's maintenance page in
    /// front of Restate, say — is neither Restate refusing nor Restate away.
    #[tokio::test]
    async fn an_unreadable_answer_from_restate_is_a_decode_error() {
        let address = restate_serving(
            "/restate/call/Batch/batch-1/progress",
            post(|| async { (StatusCode::OK, "<html>Back soon</html>") }),
        )
        .await;

        let error = ingress_at(address)
            .call::<Value>("Batch/batch-1/progress")
            .await
            .expect_err("a page is not an output");

        assert!(matches!(error, IngressError::Decode(_)), "{error:?}");
    }

    /// A refusal keeps its status, since the code is what a caller reads: a 409 says the
    /// invocation was already accepted, and that is not for this client to interpret.
    #[tokio::test]
    async fn a_refusal_carries_the_status_and_what_restate_said() {
        let address = restate_serving(
            "/restate/send/Batch/batch-1/run",
            post(|| async {
                (
                    StatusCode::CONFLICT,
                    axum::Json(json!({
                        "code": 409,
                        "message": "The invocation was previously accepted",
                        "source": "ingress"
                    })),
                )
            }),
        )
        .await;

        let error = ingress_at(address)
            .send("Batch/batch-1/run", &json!({}), Some("batch-1"))
            .await
            .expect_err("a conflict is not an acceptance");

        assert!(
            matches!(
                &error,
                IngressError::Status { code: StatusCode::CONFLICT, body }
                    if body.contains("previously accepted")
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_trailing_slash_on_the_base_is_trimmed() {
        assert_eq!(
            IngressConfig::new("http://127.0.0.1:8080/").base,
            "http://127.0.0.1:8080"
        );
    }

    #[test]
    fn a_handler_path_names_the_service_and_the_handler() {
        assert_eq!(handler_path("Scheduler", "start"), "Scheduler/start");
    }

    /// The canonical trap: a composite key must reach Restate percent-encoded, or the path
    /// stops at the `#` and addresses the object `7`.
    #[test]
    fn a_keyed_handler_path_encodes_what_would_break_the_segment() {
        assert_eq!(
            keyed_handler_path("PullRequest", "7#9", "status"),
            "PullRequest/7%239/status"
        );
        assert_eq!(
            keyed_handler_path("Repo", "acme/api", "sync"),
            "Repo/acme%2Fapi/sync"
        );
    }

    /// Unreserved text is left alone, so an ordinary key — a uuid, a number — reads in
    /// Restate's UI as the caller wrote it.
    #[test]
    fn an_unreserved_key_is_left_as_it_is() {
        let key = "0199c0ff-ee00-7000-8000-000000000001";

        assert_eq!(encode_object_key(key), key);
    }
}
