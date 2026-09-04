//! The Restate ingress client: one-way sends, request/response calls, and the
//! paths the dashboard addresses its virtual objects by.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

/// The Restate ingress this deployment enqueues work on.
///
/// Built once at startup and shared through router state, so the webhook
/// route can be exercised against a stand-in ingress. The `#[server]`
/// functions have no router state and go through [`RestateIngress::from_env`]
/// on each call instead.
#[derive(Clone)]
pub(crate) struct RestateIngress {
    pub(super) client: reqwest::Client,
    pub(super) base: String,
    pub(super) token: Option<String>,
}

impl RestateIngress {
    pub(crate) fn from_env() -> Result<Self, String> {
        let base = std::env::var("RESTATE_INGRESS_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
            .trim_end_matches('/')
            .to_owned();
        let token = std::env::var("RESTATE_AUTH_TOKEN")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::env::var("RESTATE_API_KEY")
                    .ok()
                    .filter(|value| !value.is_empty())
            });
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            base,
            token,
        })
    }

    /// Enqueues a one-way invocation, returning once Restate has accepted it.
    ///
    /// With an `idempotency_key`, Restate collapses repeats of the same key
    /// into the original invocation instead of running the handler again.
    pub(crate) async fn send<T: Serialize + ?Sized>(
        &self,
        path: &str,
        input: &T,
        idempotency_key: Option<&str>,
    ) -> Result<(), String> {
        let mut request = self.post("send", path).json(input);
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        self.accept(path, request).await
    }

    /// Enqueues a one-way invocation of a handler that takes no input.
    ///
    /// Restate rejects any body for such a handler, even an empty JSON one, so
    /// the request carries neither a body nor a content type.
    pub(crate) async fn send_empty(&self, path: &str) -> Result<(), String> {
        self.accept(path, self.post("send", path)).await
    }

    /// An authenticated POST to `/restate/{route}/{path}`, for `route` being
    /// Restate's `send` (one-way) or `call` (request/response).
    fn post(&self, route: &str, path: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .post(format!("{}/restate/{route}/{path}", self.base));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request
    }

    /// Sends a one-way invocation and reads Restate's answer as accepted or not.
    async fn accept(&self, path: &str, request: reqwest::RequestBuilder) -> Result<(), String> {
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        if status.as_u16() == 409
            && path.starts_with("BulkAction/")
            && detail.to_ascii_lowercase().contains("previously accepted")
        {
            return Ok(());
        }
        Err(format!("Restate returned {status}: {detail}"))
    }

    pub(crate) async fn call<R>(&self, path: &str) -> Result<R, String>
    where
        R: DeserializeOwned,
    {
        let response = self
            .post("call", path)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| format!("Restate returned an invalid response: {error}"))?;
        if !status.is_success() {
            return Err(format!("Restate returned {status}: {value}"));
        }
        let output = value.get("output").cloned().unwrap_or(value);
        serde_json::from_value(output).map_err(|error| error.to_string())
    }
}

pub(crate) async fn restate_send<T: Serialize + ?Sized>(
    path: &str,
    input: &T,
) -> Result<(), String> {
    RestateIngress::from_env()?.send(path, input, None).await
}

pub(crate) async fn restate_send_empty(path: &str) -> Result<(), String> {
    RestateIngress::from_env()?.send_empty(path).await
}

pub(crate) async fn restate_call<R>(path: &str) -> Result<R, String>
where
    R: DeserializeOwned,
{
    RestateIngress::from_env()?.call(path).await
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
    use dependaboard_core::PrState;

    use super::*;
    use crate::server::test_support::{ingress_at, serve};

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

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn pull_request_status_path_encodes_the_object_key_separator() {
        assert_eq!(pr_status_path(7, 9), "PullRequest/7%239/status");
    }
}
