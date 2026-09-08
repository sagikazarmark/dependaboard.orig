//! Shared fixtures for the server modules' unit tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::HeaderMap;
use dependaboard_core::{PrRecord, RepoRecord};
use dependaboard_store::{LibSqlPrStore, ProjectionWriter, StoreConfig};
use dioxus::server::{DioxusRouterExt, ServeConfig};
use secrecy::SecretString;

use crate::server::config::{Credentials, RestateConfig};
use crate::server::restate::RestateIngress;
use crate::server::router::router;
use crate::server::state::ServerState;
use crate::server::webhook::WebhookState;

/// The installation the test backend, and the dashboard served over it, is
/// bound to.
pub(crate) const INSTALLATION_ID: u64 = 42;
pub(crate) const USERNAME: &str = "dependaboard";
pub(crate) const PASSWORD: &str = "secret";
/// The webhook secret; the signed delivery in `router.rs` is made with it.
pub(crate) const WEBHOOK_SECRET: &str = "secret";

/// The HTTP client the server tests reach their loopback servers with. A
/// request the server never answers fails the test in seconds rather than
/// hanging it, and a proxy named in the environment (`HTTP_PROXY`,
/// `ALL_PROXY`) is not put between the test and its own port, as
/// `reqwest::Client::new()` would.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

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

/// What the fake Restate ingress saw for one request: a send it was asked to
/// enqueue, or a call made of it.
#[derive(Clone)]
pub(crate) struct ForwardedRequest {
    pub(crate) path: String,
    pub(crate) idempotency_key: Option<String>,
    pub(crate) body: Bytes,
}

/// Where the ingress takes a request/response call, under the handler's path.
const RESTATE_CALL: &str = "/restate/call/";

/// The stand-in Restate ingress as a test sees it: what it was sent, and
/// what it answers a call with, by the call's path.
#[derive(Clone, Default)]
pub(crate) struct FakeRestate {
    forwarded: Arc<Mutex<Vec<ForwardedRequest>>>,
    answers: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

/// Serves a stand-in for the Restate ingress that accepts every send,
/// recording what it was asked to enqueue, and answers a call with the output
/// held for its path, or with nothing, as a virtual object with no state
/// would.
pub(crate) async fn fake_restate() -> (RestateIngress, FakeRestate) {
    let fake = FakeRestate::default();
    let restate = fake.clone();
    let router = axum::Router::new().fallback(
        move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
            let restate = restate.clone();
            async move {
                let path = uri.path().to_owned();
                restate.forwarded.lock().unwrap().push(ForwardedRequest {
                    path: path.clone(),
                    idempotency_key: headers
                        .get("idempotency-key")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                    body,
                });
                if path.starts_with(RESTATE_CALL) {
                    let output = restate.answers.lock().unwrap().get(&path).cloned();
                    return axum::Json(serde_json::json!({ "output": output }));
                }
                axum::Json(serde_json::json!({
                    "invocationId": "inv_1aiqX0vFEFNH1Umgre58JiCLgHfTtztYK5",
                    "status": "Accepted"
                }))
            }
        },
    );
    let address = serve(router).await;
    (ingress_at(address), fake)
}

/// [`fake_restate`], for a test that only sends.
pub(crate) async fn fake_restate_ingress() -> (RestateIngress, Arc<Mutex<Vec<ForwardedRequest>>>) {
    let (ingress, fake) = fake_restate().await;
    (ingress, fake.forwarded)
}

/// What the server functions run against, as a test holds it without a
/// server: a [`ServerState`] over an empty in-memory read model and a
/// Restate stand-in that records what it is sent and answers what it is
/// asked with what the test put there. A server function's body is called
/// on [`Self::state`] directly; [`Dashboard`] is this served over HTTP.
pub(crate) struct Backend {
    state: ServerState,
    /// The same store the state reads, held whole so a test can write to
    /// it: the state holds only the reader half, and the test stands in for
    /// the Restate service, which does the writing.
    store: LibSqlPrStore,
    restate: FakeRestate,
}

/// A [`Backend`] over a fresh `:memory:` store and a fresh Restate stand-in,
/// for a test that calls a server function's body and looks at what it read
/// and sent.
pub(crate) async fn backend() -> Backend {
    let (ingress, restate) = fake_restate().await;
    let store = LibSqlPrStore::connect(&StoreConfig::local(":memory:"))
        .await
        .unwrap();
    let state = ServerState {
        ingress,
        store: Arc::new(store.clone()),
        installation_id: INSTALLATION_ID,
    };
    Backend {
        state,
        store,
        restate,
    }
}

impl Backend {
    /// What a server function's body takes: the state bound to
    /// [`INSTALLATION_ID`], reading the store and sending to the stand-in.
    pub(crate) fn state(&self) -> &ServerState {
        &self.state
    }

    /// Every request Restate was sent so far, in order.
    pub(crate) fn forwards(&self) -> Vec<ForwardedRequest> {
        self.restate.forwarded.lock().unwrap().clone()
    }

    /// The one request Restate was sent, which went to `path`.
    pub(crate) fn the_one_forward(&self, path: &str) -> ForwardedRequest {
        let forwards = self.forwards();
        assert_eq!(forwards.len(), 1, "one request reaches Restate");
        assert_eq!(forwards[0].path, path);
        forwards.into_iter().next().unwrap()
    }

    /// Holds `output` as what Restate answers a call to `path` with: what the
    /// handler there would return, for putting there what the Restate service
    /// would hold.
    pub(crate) fn restate_answers(&self, path: &str, output: serde_json::Value) {
        self.restate
            .answers
            .lock()
            .unwrap()
            .insert(format!("{RESTATE_CALL}{path}"), output);
    }

    /// The read model behind the state, for putting there what the Restate
    /// service would have written; a test writes to it through
    /// [`ProjectionWriter`], as the service does.
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
}

/// The whole server as a test reaches it over HTTP: the routes as `main`
/// mounts them, minus the static assets, on loopback, over a [`Backend`],
/// which it derefs to for seeding the store and reading what reached
/// Restate.
pub(crate) struct Dashboard {
    address: SocketAddr,
    backend: Backend,
}

/// A [`Dashboard`] over a fresh [`Backend`], for a test that needs the wire:
/// the auth edge, the origin policy, the route by name, the error envelope.
pub(crate) async fn dashboard() -> Dashboard {
    let backend = backend().await;
    let credentials = Credentials {
        username: USERNAME.to_owned(),
        password: SecretString::from(PASSWORD),
    };
    let webhooks = WebhookState::new(
        &SecretString::from(WEBHOOK_SECRET),
        backend.state.ingress.clone(),
    );
    let app = axum::Router::new().serve_api_application(ServeConfig::new(), crate::ui::App);
    let address = serve(router(app, credentials, backend.state.clone(), webhooks)).await;
    Dashboard { address, backend }
}

impl Deref for Dashboard {
    type Target = Backend;

    fn deref(&self) -> &Backend {
        &self.backend
    }
}

impl Dashboard {
    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
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
        client()
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
