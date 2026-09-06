//! How the server's routes fit together: the Dioxus application, its page
//! and its server functions, behind the auth edge; the GitHub webhook route
//! beside it, outside that edge, answering to GitHub's signature instead.

use axum::{Extension, middleware};

use crate::server::{
    auth::{refuse_cross_site, require_dashboard_auth},
    config::Credentials,
    state::ServerState,
    webhook::{WebhookState, webhook_router},
};

/// Mounts `app`, the Dioxus application with its server functions, behind
/// Basic Auth and the cross-site refusal, and the webhook route beside it.
/// The layers run outermost first: a request is authenticated, then held to
/// the origin policy, and only then reaches a server function with the
/// shared state attached.
pub(crate) fn router(
    app: axum::Router,
    credentials: Credentials,
    state: ServerState,
    webhooks: WebhookState,
) -> axum::Router {
    let dashboard = app
        .layer(Extension(state))
        .layer(middleware::from_fn(refuse_cross_site))
        .layer(middleware::from_fn_with_state(
            credentials,
            require_dashboard_auth,
        ));
    webhook_router(webhooks).merge(dashboard)
}

#[cfg(test)]
mod tests {
    use axum::http::header;
    use reqwest::StatusCode;

    use crate::server::test_support::{PASSWORD, USERNAME, dashboard};

    #[tokio::test]
    async fn a_request_without_credentials_is_told_how_to_authenticate() {
        let dashboard = dashboard().await;

        let response = reqwest::get(dashboard.url("/")).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Basic realm=\"dependaboard\"")
        );
    }

    /// GitHub has no dashboard password. Its route is outside the Basic Auth
    /// layer and answers to the delivery's signature instead; a delivery
    /// signed with another secret is refused without being offered the
    /// dashboard's challenge, and Restate never hears of it.
    #[tokio::test]
    async fn the_webhook_route_answers_to_githubs_signature_not_the_dashboards_credentials() {
        // openssl dgst -sha256 -hmac secret, over exactly the bytes below.
        const BODY: &[u8] = br#"{"action":"opened","installation":{"id":42}}"#;
        const SIGNATURE: &str =
            "sha256=015a17fc63d8f4eb2ffd3f3f70444a66af82856c318e854607c77d1747a3d3c9";
        const FORGED: &str =
            "sha256=0000000000000000000000000000000000000000000000000000000000000000";

        let dashboard = dashboard().await;
        let deliver = |signature: &'static str| {
            reqwest::Client::new()
                .post(dashboard.url("/api/webhooks/github"))
                .header("x-hub-signature-256", signature)
                .header("x-github-delivery", "72d3162e-cc78-11e3-81ab-4c9367dc0958")
                .header("x-github-event", "installation")
                .header(header::CONTENT_TYPE, "application/json")
                .body(BODY)
                .send()
        };

        let forged = deliver(FORGED).await.unwrap();
        assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
        assert!(forged.headers().get(header::WWW_AUTHENTICATE).is_none());
        assert!(dashboard.forwards().is_empty());

        let signed = deliver(SIGNATURE).await.unwrap();
        assert_eq!(signed.status(), StatusCode::OK);
        dashboard.the_one_forward("/restate/send/WebhookIngress/dispatch");
    }

    /// A page on another site can make the browser POST here with the
    /// dashboard's stored credentials attached. The credentials get it past
    /// authentication; the origin does not get it any further.
    #[tokio::test]
    async fn a_state_change_another_site_asked_for_is_refused_despite_the_credentials() {
        let dashboard = dashboard().await;

        let cross_site = reqwest::Client::new()
            .post(dashboard.server_fn("request_sync"))
            .basic_auth(USERNAME, Some(PASSWORD))
            .header("sec-fetch-site", "cross-site")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
        assert!(dashboard.forwards().is_empty());

        let own = dashboard
            .call("request_sync", serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(own.status(), StatusCode::OK);
        dashboard.the_one_forward("/restate/send/DashboardIngress/sync_installation");
    }
}
