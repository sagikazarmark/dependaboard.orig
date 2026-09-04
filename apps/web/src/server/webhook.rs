//! The GitHub webhook edge: verifies each delivery, reduces it to the routing
//! fields `WebhookIngress` dispatches on, and forwards it to Restate under the
//! delivery id so redeliveries collapse into one invocation.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use dependaboard_core::WebhookEvent;
use octoevents::{Envelope, EventKind, ResponseStatus, Secret, Verifier};
use serde::{Deserialize, de::DeserializeOwned};

use crate::server::restate::RestateIngress;

/// Builds the webhook verifier once, so a missing secret fails at startup
/// rather than at the first delivery.
pub(crate) fn webhook_verifier() -> Verifier {
    let secret =
        std::env::var("GITHUB_WEBHOOK_SECRET").expect("GITHUB_WEBHOOK_SECRET must be configured");
    assert!(
        !secret.trim().is_empty(),
        "GITHUB_WEBHOOK_SECRET must not be empty"
    );
    Verifier::new(Secret::new(secret))
}

#[derive(Clone)]
pub(crate) struct WebhookState {
    pub(crate) verifier: Verifier,
    pub(crate) ingress: RestateIngress,
}

pub(crate) fn webhook_router(state: WebhookState) -> axum::Router {
    axum::Router::new()
        .route("/api/webhooks/github", post(github_webhook))
        .with_state(state)
}

async fn github_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let envelope = match Envelope::from_signed_parts(&state.verifier, &headers, body) {
        Ok(envelope) => envelope,
        Err(error) => {
            tracing::warn!(%error, "GitHub webhook delivery was rejected");
            let status = StatusCode::from(ResponseStatus::for_receive_error(&error));
            return (status, "webhook delivery was rejected").into_response();
        }
    };
    let event = match route_delivery(&envelope) {
        Ok(Disposition::Forward(event)) => event,
        Ok(Disposition::Acknowledge) => return StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::warn!(%error, event = %envelope.kind, "GitHub webhook payload was rejected");
            return (StatusCode::BAD_REQUEST, "invalid GitHub webhook payload").into_response();
        }
    };
    // A redelivery from the App's delivery log replays the request under the
    // same delivery id, so it is the key that lets Restate fold the replay
    // into the dispatch it already accepted.
    match state
        .ingress
        .send(
            "WebhookIngress/dispatch",
            &event,
            Some(&envelope.delivery_id),
        )
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::error!(%error, event = %envelope.kind, "Restate rejected webhook");
            (StatusCode::BAD_GATEWAY, "could not enqueue webhook").into_response()
        }
    }
}

/// What the edge does with a verified delivery.
#[derive(Debug)]
enum Disposition {
    /// `WebhookIngress::dispatch` has an arm for this kind; forward it.
    ///
    /// Boxed so the whole enum is not sized by this one large variant.
    Forward(Box<WebhookEvent>),
    /// Nothing downstream routes this kind, so a forward would only buy an
    /// invocation that drops the event. Acknowledge it to GitHub and stop.
    Acknowledge,
}

/// Decides what the edge does with a verified delivery: forward it, reduced
/// to the routing fields `WebhookIngress` dispatches on, or acknowledge it.
///
/// The kinds matched here are the edge's copy of the dispatcher's routing
/// table and must stay in step with `WebhookIngress::dispatch`: a kind added
/// there but not here is acknowledged at the edge and never reaches Restate.
///
/// The envelope already carries the installation and repository probe, so only
/// the per-event fields — PR number, head SHA, and the PRs a check belongs to —
/// need parsing out of the payload.
fn route_delivery(envelope: &Envelope) -> Result<Disposition, String> {
    let (number, sha, pull_requests) = match envelope.kind {
        EventKind::Installation | EventKind::InstallationRepositories => {
            if envelope.common.installation_id.is_none() {
                return Err("GitHub installation webhook is missing an installation id".to_owned());
            }
            (None, None, Vec::new())
        }
        EventKind::PullRequest => {
            let payload: PullRequestRouting = parse_payload(envelope)?;
            (
                Some(payload.pull_request.number),
                Some(payload.pull_request.head.sha),
                Vec::new(),
            )
        }
        EventKind::CheckRun => {
            let payload: CheckRunRouting = parse_payload(envelope)?;
            let (sha, numbers) = payload.check_run.into_routing();
            (None, Some(sha), numbers)
        }
        EventKind::CheckSuite => {
            let payload: CheckSuiteRouting = parse_payload(envelope)?;
            let (sha, numbers) = payload.check_suite.into_routing();
            (None, Some(sha), numbers)
        }
        EventKind::Status => {
            let payload: StatusRouting = parse_payload(envelope)?;
            (None, Some(payload.sha), Vec::new())
        }
        _ => return Ok(Disposition::Acknowledge),
    };
    let repository = match envelope.kind {
        EventKind::PullRequest
        | EventKind::CheckRun
        | EventKind::CheckSuite
        | EventKind::Status => Some(
            envelope
                .common
                .repository
                .as_ref()
                .ok_or("GitHub repository webhook is missing routing fields")?,
        ),
        _ => envelope.common.repository.as_ref(),
    };
    Ok(Disposition::Forward(Box::new(WebhookEvent {
        event: envelope.kind.as_str().to_owned(),
        action: envelope
            .action
            .as_ref()
            .map(|action| action.as_str().to_owned()),
        installation_id: envelope.common.installation_id,
        repository_id: repository.map(|repository| repository.id),
        owner: repository.map(|repository| repository.owner.clone()),
        repo: repository.map(|repository| repository.name.clone()),
        number,
        sha,
        pull_requests,
    })))
}

fn parse_payload<T: DeserializeOwned>(envelope: &Envelope) -> Result<T, String> {
    envelope.parse::<T>().map_err(|error| error.to_string())
}

#[derive(Deserialize)]
struct PullRequestRouting {
    pull_request: PullRequestRef,
}

#[derive(Deserialize)]
struct PullRequestRef {
    number: u64,
    head: CommitRef,
}

#[derive(Deserialize)]
struct CommitRef {
    sha: String,
}

#[derive(Deserialize)]
struct CheckRunRouting {
    check_run: CheckRouting,
}

#[derive(Deserialize)]
struct CheckSuiteRouting {
    check_suite: CheckRouting,
}

#[derive(Deserialize)]
struct StatusRouting {
    sha: String,
}

#[derive(Deserialize)]
struct CheckRouting {
    head_sha: String,
    #[serde(default)]
    pull_requests: Vec<CheckPullRequest>,
}

impl CheckRouting {
    fn into_routing(self) -> (String, Vec<u64>) {
        let numbers = self
            .pull_requests
            .into_iter()
            .map(|pull| pull.number)
            .collect();
        (self.head_sha, numbers)
    }
}

#[derive(Deserialize)]
struct CheckPullRequest {
    number: u64,
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::http::header;
    use serde_json::Value;

    use super::*;
    use crate::server::test_support::{ingress_at, serve};

    #[test]
    fn webhook_deliveries_are_authenticated_before_they_are_routed() {
        // openssl dgst -sha256 -hmac secret, over exactly the bytes below.
        const BODY: &[u8] = br#"{"action":"opened","installation":{"id":42}}"#;
        const SIGNATURE: &str =
            "sha256=015a17fc63d8f4eb2ffd3f3f70444a66af82856c318e854607c77d1747a3d3c9";

        let body = Bytes::from_static(BODY);
        let headers = |signature: &'static str| {
            octoevents::HeaderView::new()
                .signature(signature)
                .delivery_id("72d3162e-cc78-11e3-81ab-4c9367dc0958")
                .event_name("installation")
                .content_type("application/json")
        };

        let envelope = Envelope::from_signed(
            &Verifier::new(Secret::new("secret")),
            &headers(SIGNATURE),
            body.clone(),
        )
        .expect("a correctly signed delivery is accepted");
        assert_eq!(envelope.kind, EventKind::Installation);
        assert_eq!(envelope.common.installation_id, Some(42));
        let Ok(Disposition::Forward(event)) = route_delivery(&envelope) else {
            panic!("a verified installation delivery is forwarded");
        };
        assert_eq!(event.installation_id, Some(42));

        let mismatched = Envelope::from_signed(
            &Verifier::new(Secret::new("wrong")),
            &headers(SIGNATURE),
            body.clone(),
        )
        .expect_err("a delivery signed with another secret is refused");
        assert_eq!(
            ResponseStatus::for_receive_error(&mismatched),
            ResponseStatus::Unauthorized
        );

        let malformed = Envelope::from_signed(
            &Verifier::new(Secret::new("secret")),
            &headers("sha1=abcd"),
            body,
        )
        .expect_err("a signature that is not sha256 hexadecimal is refused");
        assert_eq!(
            ResponseStatus::for_receive_error(&malformed),
            ResponseStatus::BadRequest
        );
    }

    #[tokio::test]
    async fn webhook_forwards_carry_the_delivery_id_as_the_idempotency_key() {
        // openssl dgst -sha256 -hmac secret, over exactly the bytes below.
        const BODY: &[u8] = br#"{"action":"synchronize","number":9,"pull_request":{"number":9,"head":{"sha":"abc123"}},"repository":{"id":7,"name":"api","full_name":"acme/api","owner":{"login":"acme"}},"installation":{"id":42}}"#;
        const SIGNATURE: &str =
            "sha256=842b71b366f883f03823c869358f15ebccdafeb019fe56b98e5b68824c83617e";
        const DELIVERY_ID: &str = "72d3162e-cc78-11e3-81ab-4c9367dc0958";

        let (ingress, forwarded) = fake_restate_ingress().await;
        let webhook = serve(webhook_router(WebhookState {
            verifier: Verifier::new(Secret::new("secret")),
            ingress,
        }))
        .await;

        // A delivery that timed out at GitHub's 10 s is marked failed even if
        // the forward was accepted, and redelivering it replays the request
        // under the same delivery id. Both forwards must reach Restate under
        // that key so Restate can collapse them into one dispatch invocation.
        for _ in 0..2 {
            let response = deliver(webhook, "pull_request", DELIVERY_ID, SIGNATURE, BODY).await;
            assert_eq!(response.status(), reqwest::StatusCode::OK);
        }

        let forwarded = forwarded.lock().unwrap();
        assert_eq!(forwarded.len(), 2);
        for forward in forwarded.iter() {
            assert_eq!(forward.path, "/restate/send/WebhookIngress/dispatch");
            assert_eq!(forward.idempotency_key.as_deref(), Some(DELIVERY_ID));
            let event: WebhookEvent = serde_json::from_slice(&forward.body).unwrap();
            assert_eq!(event.event, "pull_request");
            assert_eq!(event.action.as_deref(), Some("synchronize"));
            assert_eq!(event.installation_id, Some(42));
            assert_eq!(event.repository_id, Some(7));
            assert_eq!(event.number, Some(9));
            assert_eq!(event.sha.as_deref(), Some("abc123"));
        }
    }

    #[tokio::test]
    async fn unroutable_webhook_kinds_are_acknowledged_without_reaching_restate() {
        // openssl dgst -sha256 -hmac secret, over exactly the bytes below.
        const BODY: &[u8] = br#"{"ref":"refs/heads/main","repository":{"id":7,"name":"api","full_name":"acme/api","owner":{"login":"acme"}},"installation":{"id":42}}"#;
        const SIGNATURE: &str =
            "sha256=a95cdd79c4af2a80adfd8e387959a62f14bb707da7ee6c1172137bf2d0e397c4";

        let (ingress, forwarded) = fake_restate_ingress().await;
        let webhook = serve(webhook_router(WebhookState {
            verifier: Verifier::new(Secret::new("secret")),
            ingress,
        }))
        .await;

        let response = deliver(
            webhook,
            "push",
            "9b6c1f52-6e2a-4a0e-9d0f-2c4b8a1e7f30",
            SIGNATURE,
            BODY,
        )
        .await;

        // GitHub only wants a 2xx so the delivery does not show up as failed;
        // Restate should never hear about it.
        assert!(response.status().is_success(), "{}", response.status());
        assert!(forwarded.lock().unwrap().is_empty());
    }

    /// What the fake Restate ingress saw for one `/restate/send` request.
    struct ForwardedSend {
        path: String,
        idempotency_key: Option<String>,
        body: Bytes,
    }

    /// Serves a stand-in for the Restate ingress that accepts every send and
    /// records what it was asked to enqueue.
    async fn fake_restate_ingress() -> (RestateIngress, Arc<Mutex<Vec<ForwardedSend>>>) {
        let forwarded = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&forwarded);
        let router = axum::Router::new().fallback(
            move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
                let recorder = Arc::clone(&recorder);
                async move {
                    recorder.lock().unwrap().push(ForwardedSend {
                        path: uri.path().to_owned(),
                        idempotency_key: headers
                            .get("idempotency-key")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned),
                        body,
                    });
                    axum::Json(serde_json::json!({
                        "invocationId": "inv_1aiqX0vFEFNH1Umgre58JiCLgHfTtztYK5",
                        "status": "Accepted"
                    }))
                }
            },
        );
        let address = serve(router).await;
        (ingress_at(address), forwarded)
    }

    /// Posts a delivery the way GitHub does: signed, typed, and identified.
    async fn deliver(
        webhook: SocketAddr,
        event_name: &str,
        delivery_id: &str,
        signature: &str,
        body: &'static [u8],
    ) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("http://{webhook}/api/webhooks/github"))
            .header("x-hub-signature-256", signature)
            .header("x-github-delivery", delivery_id)
            .header("x-github-event", event_name)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    #[test]
    fn verified_envelopes_are_reduced_to_routing_fields() {
        let installation = routed(
            "installation",
            Some("created"),
            false,
            serde_json::json!({ "action": "created", "repositories": [] }),
        );
        assert_eq!(installation.action.as_deref(), Some("created"));
        assert_eq!(installation.installation_id, Some(42));
        assert_eq!(installation.repository_id, None);

        let repositories = routed(
            "installation_repositories",
            Some("removed"),
            true,
            serde_json::json!({
                "action": "removed",
                "repositories_added": [],
                "repositories_removed": [],
                "repository_selection": "all"
            }),
        );
        assert_eq!(repositories.action.as_deref(), Some("removed"));

        let pull_request = routed(
            "pull_request",
            Some("synchronize"),
            true,
            serde_json::json!({
                "action": "synchronize",
                "number": 9,
                "pull_request": {
                    "number": 9,
                    "head": { "ref": "dependabot/update", "sha": "abc123" },
                    "base": { "ref": "main", "sha": "base123" }
                }
            }),
        );
        assert_eq!(pull_request.number, Some(9));
        assert_eq!(pull_request.sha.as_deref(), Some("abc123"));

        for object_name in ["check_run", "check_suite"] {
            let mut payload = serde_json::json!({ "action": "completed" });
            payload[object_name] = serde_json::json!({
                "head_sha": "abc123",
                "pull_requests": [{ "number": 9 }, { "number": 10 }]
            });
            let event = routed(object_name, Some("completed"), true, payload);
            assert_eq!(event.action.as_deref(), Some("completed"));
            assert_eq!(event.sha.as_deref(), Some("abc123"));
            assert_eq!(event.pull_requests, vec![9, 10]);
        }

        let status = routed(
            "status",
            None,
            true,
            serde_json::json!({
                "context": "ci/test",
                "id": 1,
                "name": "ci/test",
                "sha": "abc123",
                "state": "success"
            }),
        );
        assert_eq!(status.sha.as_deref(), Some("abc123"));
        assert_eq!(status.action, None);

        for event in [repositories, pull_request, status] {
            assert_eq!(event.installation_id, Some(42));
            assert_eq!(event.repository_id, Some(7));
            assert_eq!(event.owner.as_deref(), Some("acme"));
            assert_eq!(event.repo.as_deref(), Some("api"));
        }
    }

    #[test]
    fn envelope_missing_routing_fields_is_rejected() {
        // Authenticated but unroutable: the payload verified, yet nothing
        // downstream can address a pull request or repository with it.
        assert!(
            route_delivery(&envelope(
                "pull_request",
                Some("opened"),
                true,
                &serde_json::json!({ "action": "opened" })
            ))
            .is_err()
        );
        let mut anonymous = envelope(
            "installation",
            Some("created"),
            false,
            &serde_json::json!({ "action": "created" }),
        );
        anonymous.common.installation_id = None;
        assert!(route_delivery(&anonymous).is_err());

        let mut check_run = serde_json::json!({
            "action": "completed",
            "check_run": { "head_sha": "abc123", "pull_requests": [{}] }
        });
        assert!(
            route_delivery(&envelope("check_run", Some("completed"), true, &check_run)).is_err()
        );
        check_run["check_run"]["pull_requests"] = serde_json::json!([]);
        check_run["check_run"]
            .as_object_mut()
            .unwrap()
            .remove("head_sha");
        assert!(
            route_delivery(&envelope("check_run", Some("completed"), true, &check_run)).is_err()
        );

        let pull_request = serde_json::json!({
            "action": "opened",
            "number": 9,
            "pull_request": { "number": 9, "head": { "sha": "abc123" } }
        });
        assert!(
            route_delivery(&envelope(
                "pull_request",
                Some("opened"),
                false,
                &pull_request
            ))
            .is_err()
        );
    }

    #[test]
    fn kinds_the_dispatcher_does_not_route_are_acknowledged_at_the_edge() {
        // Verified deliveries whose kind has no arm in `WebhookIngress::dispatch`
        // are acknowledged here instead of costing a Restate invocation that
        // would only drop them. `ping` is the everyday case: GitHub sends it
        // when the App is installed, before any real delivery.
        for (event_name, payload) in [
            (
                "ping",
                serde_json::json!({ "zen": "Keep it logically awesome." }),
            ),
            ("push", serde_json::json!({ "ref": "refs/heads/main" })),
            (
                "issue_comment",
                serde_json::json!({ "action": "created", "issue": { "number": 9 } }),
            ),
            (
                "some_future_event",
                serde_json::json!({ "action": "created" }),
            ),
        ] {
            let disposition = route_delivery(&envelope(event_name, None, true, &payload))
                .unwrap_or_else(|error| panic!("{event_name}: {error}"));
            assert!(
                matches!(disposition, Disposition::Acknowledge),
                "{event_name} should be acknowledged without a forward"
            );
        }
    }

    fn routed(
        event_name: &str,
        action: Option<&str>,
        repository: bool,
        payload: Value,
    ) -> WebhookEvent {
        match route_delivery(&envelope(event_name, action, repository, &payload)).unwrap() {
            Disposition::Forward(event) => *event,
            Disposition::Acknowledge => panic!("{event_name} should be forwarded"),
        }
    }

    /// Builds the synthetic envelope a verified delivery would produce.
    ///
    /// `octoevents` extracts `common` from the payload itself, so the probe is
    /// mirrored here rather than re-derived: these tests cover this crate's
    /// routing, not the crate's extraction.
    fn envelope(
        event_name: &str,
        action: Option<&str>,
        repository: bool,
        payload: &Value,
    ) -> Envelope {
        let mut common = octoevents::Common::default();
        common.installation_id = Some(42);
        if repository {
            let mut reference = octoevents::RepositoryRef::default();
            reference.id = 7;
            reference.name = "api".to_owned();
            reference.full_name = "acme/api".to_owned();
            reference.owner = "acme".to_owned();
            common.repository = Some(reference);
        }
        Envelope {
            delivery_id: "72d3162e-cc78-11e3-81ab-4c9367dc0958".to_owned(),
            kind: event_name.parse().unwrap(),
            action: action.map(|action| action.parse().unwrap()),
            common,
            target_type: None,
            target_id: None,
            raw: Bytes::from(serde_json::to_vec(payload).unwrap()),
        }
    }
}
