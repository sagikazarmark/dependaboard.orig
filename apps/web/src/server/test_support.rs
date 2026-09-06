//! Shared fixtures for the server modules' unit tests.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::HeaderMap;
use dependaboard_core::{PrRecord, RepoRecord};
use dependaboard_store::{LibSqlPrStore, PrStore, StoreConfig};
use dioxus::server::{DioxusRouterExt, ServeConfig};
use secrecy::SecretString;

use crate::server::config::{Credentials, RestateConfig};
use crate::server::restate::RestateIngress;
use crate::server::router::router;
use crate::server::state::ServerState;
use crate::server::webhook::WebhookState;

/// The installation the test dashboard is bound to.
pub(crate) const INSTALLATION_ID: u64 = 42;
pub(crate) const USERNAME: &str = "dependaboard";
pub(crate) const PASSWORD: &str = "secret";
/// The webhook secret; the signed delivery in `router.rs` is made with it.
pub(crate) const WEBHOOK_SECRET: &str = "secret";

/// A client for a Restate ingress stand-in listening at `address`.
pub(crate) fn ingress_at(address: SocketAddr) -> RestateIngress {
    RestateIngress::new(RestateConfig {
        base: format!("http://{address}"),
        token: None,
    })
    .unwrap()
}

/// Serves `router` on a loopback port for the rest of the test; the task
/// is dropped with the test runtime.
pub(crate) async fn serve(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    address
}

/// What the fake Restate ingress saw for one `/restate/send` request.
#[derive(Clone)]
pub(crate) struct ForwardedSend {
    pub(crate) path: String,
    pub(crate) idempotency_key: Option<String>,
    pub(crate) body: Bytes,
}

/// Serves a stand-in for the Restate ingress that accepts every send and
/// records what it was asked to enqueue.
pub(crate) async fn fake_restate_ingress() -> (RestateIngress, Arc<Mutex<Vec<ForwardedSend>>>) {
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

/// The whole server as a test reaches it over HTTP: the routes as `main`
/// mounts them, minus the static assets, on loopback; behind them an empty
/// in-memory read model and a Restate stand-in that records what it is sent.
pub(crate) struct Dashboard {
    address: SocketAddr,
    store: LibSqlPrStore,
    forwarded: Arc<Mutex<Vec<ForwardedSend>>>,
}

pub(crate) async fn dashboard() -> Dashboard {
    let (ingress, forwarded) = fake_restate_ingress().await;
    let store = LibSqlPrStore::connect(&StoreConfig::local(":memory:"))
        .await
        .unwrap();
    let state = ServerState {
        ingress: ingress.clone(),
        store: store.clone(),
        installation_id: INSTALLATION_ID,
    };
    let credentials = Credentials {
        username: USERNAME.to_owned(),
        password: SecretString::from(PASSWORD),
    };
    let webhooks = WebhookState::new(&SecretString::from(WEBHOOK_SECRET), ingress);
    let app = axum::Router::new().serve_api_application(ServeConfig::new(), crate::ui::App);
    let address = serve(router(app, credentials, state, webhooks)).await;
    Dashboard {
        address,
        store,
        forwarded,
    }
}

impl Dashboard {
    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// Every request Restate was sent so far, in order.
    pub(crate) fn forwards(&self) -> Vec<ForwardedSend> {
        self.forwarded.lock().unwrap().clone()
    }

    /// The one request Restate was sent, which went to `path`.
    pub(crate) fn the_one_forward(&self, path: &str) -> ForwardedSend {
        let forwards = self.forwards();
        assert_eq!(forwards.len(), 1, "one request reaches Restate");
        assert_eq!(forwards[0].path, path);
        forwards.into_iter().next().unwrap()
    }

    /// The read model behind the server, for putting there what the Restate
    /// service would have written.
    pub(crate) fn store(&self) -> &LibSqlPrStore {
        &self.store
    }

    /// Puts `row` in the read model, under a repository of `installation_id`:
    /// the repository row is what says which installation a pull request
    /// belongs to.
    pub(crate) async fn project(&self, installation_id: u64, row: &PrRecord) {
        self.store
            .upsert_repo(&RepoRecord {
                repository_id: row.repository_id,
                installation_id,
                owner: row.owner.clone(),
                repo: row.repo.clone(),
                merge_method: None,
                synced_at: row.synced_at,
            })
            .await
            .unwrap();
        self.store.upsert_pr(row).await.unwrap();
    }

    /// The URL a server function is mounted at. Dioxus suffixes the name with
    /// a hash of the module, so the route is found by the name, not spelled.
    pub(crate) fn server_fn(&self, name: &str) -> String {
        let prefix = format!("/api/{name}");
        let path = dioxus::server::ServerFunction::collect()
            .into_iter()
            .map(|function| function.path())
            .find(|path| {
                path.strip_prefix(&prefix)
                    .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
            })
            .unwrap_or_else(|| panic!("no server function named {name}"));
        self.url(path)
    }

    /// Calls `name` the way the dashboard's own scripts do: with the
    /// credentials, from the dashboard's origin, `arguments` as the JSON body.
    pub(crate) fn call(&self, name: &str, arguments: serde_json::Value) -> reqwest::RequestBuilder {
        self.call_as(name, PASSWORD, arguments)
    }

    /// [`Self::call`] with `password` in place of the one the server expects:
    /// what a tab left open across a rotation sends.
    pub(crate) fn call_as(
        &self,
        name: &str,
        password: &str,
        arguments: serde_json::Value,
    ) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .post(self.server_fn(name))
            .basic_auth(USERNAME, Some(password))
            .header("sec-fetch-site", "same-origin")
            .json(&arguments)
    }
}

/// The message a failed server function answered with, out of the envelope
/// Dioxus wraps a [`dioxus::prelude::ServerFnError`] in on the wire.
pub(crate) async fn error_message(response: reqwest::Response) -> String {
    let body: serde_json::Value = response.json().await.unwrap();
    body["data"]["ServerError"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("an error response: {body}"))
        .to_owned()
}
