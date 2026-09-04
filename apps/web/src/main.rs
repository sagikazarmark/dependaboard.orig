//! Entry point for the dashboard: the server binary mounts the Dioxus app
//! behind basic auth next to the GitHub webhook route; the wasm build launches
//! the app.

#![allow(non_snake_case)]

mod api;
// Installed from the dioxus-daisyui-components registry with `dx components add`;
// each component carries its full daisyUI axis, so unused variants are expected.
#[allow(dead_code, unused_imports)]
mod components;
#[cfg(feature = "server")]
mod server;
mod ui;

#[cfg(feature = "server")]
use {
    crate::server::{
        auth::require_dashboard_auth,
        restate::RestateIngress,
        webhook::{WebhookState, webhook_router, webhook_verifier},
    },
    axum::middleware,
    dioxus::server::{DioxusRouterExt, ServeConfig},
};

#[cfg(feature = "server")]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dependaboard_web=info,tower_http=info".into()),
        )
        .init();

    let address = dioxus::cli_config::fullstack_address_or_localhost();
    let dashboard_password =
        std::env::var("DASHBOARD_PASSWORD").expect("DASHBOARD_PASSWORD must be configured");
    assert!(
        !dashboard_password.trim().is_empty(),
        "DASHBOARD_PASSWORD must not be empty"
    );
    let dashboard = axum::Router::new()
        .serve_dioxus_application(ServeConfig::new(), ui::App)
        .layer(middleware::from_fn(require_dashboard_auth));
    let webhooks = webhook_router(WebhookState {
        verifier: webhook_verifier(),
        ingress: RestateIngress::from_env().expect("Restate ingress client should build"),
    });
    let router = webhooks.merge(dashboard);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .expect("web listener should bind");
    tracing::info!(%address, "dependaboard web listening");
    axum::serve(listener, router)
        .await
        .expect("web server should run");
}

#[cfg(not(feature = "server"))]
fn main() {
    dioxus::launch(ui::App);
}
