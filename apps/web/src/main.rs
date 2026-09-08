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
        config::Config, restate::RestateIngress, router::router, state::ServerState,
        webhook::WebhookState,
    },
    dependaboard_store::{LibSqlPrStore, StoreConfig},
    dioxus::server::{DioxusRouterExt, ServeConfig},
    std::sync::Arc,
};

#[cfg(feature = "server")]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dependaboard_web=info".into()),
        )
        .init();

    let config = Config::from_env().unwrap_or_else(|error| panic!("{error}"));
    let address = dioxus::cli_config::fullstack_address_or_localhost();
    let ingress = RestateIngress::new(config.restate).expect("Restate ingress client should build");
    let store = LibSqlPrStore::connect(&StoreConfig::from_env())
        .await
        .expect("read-model store should connect");
    let state = ServerState {
        ingress: ingress.clone(),
        store: Arc::new(store),
        installation_id: config.installation_id,
    };
    let app = axum::Router::new().serve_dioxus_application(ServeConfig::new(), ui::App);
    let webhooks = WebhookState::new(&config.webhook_secret, ingress);
    let router = router(app, config.credentials, state, webhooks);
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
