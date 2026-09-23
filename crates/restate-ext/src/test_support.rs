//! Fixtures for testing handlers and callers written against this crate: a deterministic
//! clock, a log sink, and a stand-in for Restate's ingress.
//!
//! Behind the `test-support` feature, so a service does not carry them into its binary. The
//! crate's own tests see the module through `cfg(test)`.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode, Uri},
};
use restate_sdk::service::Discoverable;

use crate::ingress::{IngressClient, IngressConfig};

/// The fake clock's first reading, in Unix seconds.
pub const CLOCK_EPOCH: u64 = 1_700_000_000;

/// The clock a handler's recorded effects read, for a test that cares when something
/// happened relative to something else rather than what the wall clock said.
///
/// One reading per second from [`CLOCK_EPOCH`], so what a handler did first is visible in
/// what it stamped, and a fence read before a listing is strictly smaller than one read
/// after it. One epoch and one tick across every handler's fake, so two tests that stamp the
/// same order read alike.
///
/// It also keeps the names the reads were journaled under. A clock reading is a journaled
/// step and `name` is what a replay matches it by, so two reads in one invocation under one
/// name would replay the first reading for both — the fake records the names so a test can
/// say they are distinct.
#[derive(Default)]
pub struct FakeClock {
    readings: u64,
    steps: Vec<&'static str>,
}

impl FakeClock {
    /// The next reading, journaled under `step`.
    pub fn now(&mut self, step: &'static str) -> u64 {
        self.steps.push(step);
        let reading = CLOCK_EPOCH + self.readings;
        self.readings += 1;
        reading
    }

    /// What the next reading will be, without taking it.
    pub fn peek(&self) -> u64 {
        CLOCK_EPOCH + self.readings
    }

    /// Lets `duration` pass, as a durable sleep between two readings does.
    pub fn wait(&mut self, duration: Duration) {
        self.readings += duration.as_secs();
    }

    /// The steps the handler journaled its clock reads under, in order.
    pub fn steps(&self) -> &[&'static str] {
        &self.steps
    }
}

/// An `io::Write` the test subscriber can hand out repeatedly.
#[derive(Clone, Default)]
pub struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogSink {
    pub fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
        let sink = self.clone();
        tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish()
    }

    pub fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Everything logged while `run` executes, as the operator would see it. For an async
/// attempt, use [`LogSink::subscriber`] with `WithSubscriber` instead.
pub fn captured_logs(run: impl FnOnce()) -> String {
    let sink = LogSink::default();
    tracing::subscriber::with_default(sink.subscriber(), run);
    sink.contents()
}

/// Asserts a service is reachable through the ingress under the names a caller addresses it
/// by: the service name, and each handler, all public.
///
/// The point is a caller in another process, which builds its paths from constants rather
/// than from the service's Rust types: a handler renamed here without the constant is a path
/// the caller would still be sending to, and Restate would answer with a 404 that a
/// [`call_optional`](crate::ingress::IngressClient::call_optional) reads as "no such
/// invocation". Hold both ends to the same constants and assert them here.
pub fn assert_ingress_handlers<S: Discoverable>(service: &str, handlers: &[&str]) {
    let discovery = S::discover();
    assert_eq!(discovery.name.as_str(), service);
    assert_ne!(
        discovery.ingress_private,
        Some(true),
        "{service} is not reachable through the ingress"
    );

    for handler_name in handlers {
        let handler = discovery
            .handlers
            .iter()
            .find(|handler| handler.name.as_str() == *handler_name)
            .unwrap_or_else(|| panic!("missing {service}/{handler_name}"));
        assert_ne!(handler.ingress_private, Some(true), "{handler_name}");
    }
}

/// A client for a Restate ingress stand-in listening at `address`.
pub fn ingress_at(address: SocketAddr) -> IngressClient {
    IngressClient::new(IngressConfig::new(format!("http://{address}"))).unwrap()
}

/// A loopback port nobody listens on: what a stopped Restate looks like from a caller.
/// Bound and released, so it was free a moment ago; nothing in a suite binds a port it did
/// not just get from the kernel.
pub fn closed_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// Serves `router` on a loopback port for the rest of the test; the task is dropped with the
/// test runtime.
pub async fn serve(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    address
}

/// What the fake ingress saw for one request: a send it was asked to enqueue, or a call made
/// of it.
#[derive(Clone, Debug)]
pub struct ForwardedRequest {
    /// The full ingress path, e.g. `/restate/send/PullRequest/7%239/sync`.
    pub path: String,
    pub idempotency_key: Option<String>,
    pub body: Bytes,
}

/// Where the ingress takes a request/response call, under the handler's path.
const RESTATE_CALL: &str = "/restate/call/";

/// The stand-in Restate ingress as a test holds it: what it was sent, what it answers a call
/// with, by the call's path, and which paths it refuses.
#[derive(Clone, Default)]
pub struct FakeIngress {
    forwarded: Arc<Mutex<Vec<ForwardedRequest>>>,
    answers: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    refusals: Arc<Mutex<HashMap<String, StatusCode>>>,
}

/// Serves a stand-in for the Restate ingress that accepts every send, recording what it was
/// asked to enqueue, and answers a call with the output held for its path, or with nothing,
/// as a virtual object with no state would — unless the path is one it has been told to
/// refuse, which it answers with the status held for it, in the shape Restate's ingress
/// refuses in.
///
/// For a test that asserts a *path* rather than what was sent to it, serve a router with the
/// one route instead: anything else is then a 404, and the call succeeding is the assertion.
pub async fn fake_ingress() -> (IngressClient, FakeIngress) {
    let fake = FakeIngress::default();
    let restate = fake.clone();
    let router = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
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
            if let Some(status) = restate.refusals.lock().unwrap().get(&path).copied() {
                return (
                    status,
                    axum::Json(serde_json::json!({
                        "code": status.as_u16(),
                        "message": status.canonical_reason().unwrap_or_default(),
                        "source": "ingress"
                    })),
                );
            }
            if path.starts_with(RESTATE_CALL) {
                let output = restate.answers.lock().unwrap().get(&path).cloned();
                return (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({ "output": output })),
                );
            }
            (
                StatusCode::ACCEPTED,
                axum::Json(serde_json::json!({
                    "invocationId": "inv_1aiqX0vFEFNH1Umgre58JiCLgHfTtztYK5",
                    "status": "Accepted"
                })),
            )
        }
    });
    let address = serve(router).await;
    (ingress_at(address), fake)
}

impl FakeIngress {
    /// Every request the ingress was sent so far, in order.
    pub fn forwards(&self) -> Vec<ForwardedRequest> {
        self.forwarded.lock().unwrap().clone()
    }

    /// The one request the ingress was sent, which went to `path` — the full ingress path,
    /// as [`ForwardedRequest::path`] carries it.
    pub fn the_one_forward(&self, path: &str) -> ForwardedRequest {
        let forwards = self.forwards();
        assert_eq!(forwards.len(), 1, "one request reaches Restate");
        assert_eq!(forwards[0].path, path);
        forwards.into_iter().next().unwrap()
    }

    /// Holds `output` as what the ingress answers a call to `path` with: what the handler
    /// there would return, for putting there what the Restate service would hold. `path` is
    /// the handler path, without the `/restate/call/` prefix.
    pub fn answers(&self, path: &str, output: serde_json::Value) {
        self.answers
            .lock()
            .unwrap()
            .insert(format!("{RESTATE_CALL}{path}"), output);
    }

    /// Has the ingress refuse a call to `path` with `status`, as Restate's does: a 404 for an
    /// invocation it never had, a 401 for a key it does not accept.
    pub fn refuses(&self, path: &str, status: StatusCode) {
        self.refusals
            .lock()
            .unwrap()
            .insert(format!("{RESTATE_CALL}{path}"), status);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// A reading per second, so what a handler stamped first is visibly earlier, and a wait
    /// moves the clock by what it waited.
    #[test]
    fn the_fake_clock_reads_one_second_per_step_and_keeps_their_names() {
        let mut clock = FakeClock::default();

        assert_eq!(clock.now("fence"), CLOCK_EPOCH);
        assert_eq!(clock.peek(), CLOCK_EPOCH + 1);
        assert_eq!(clock.now("stamp"), CLOCK_EPOCH + 1);
        clock.wait(Duration::from_secs(60));

        assert_eq!(clock.now("after_the_wait"), CLOCK_EPOCH + 62);
        assert_eq!(clock.steps(), ["fence", "stamp", "after_the_wait"]);
    }

    #[tokio::test]
    async fn the_fake_ingress_records_a_send_with_its_idempotency_key() {
        let (ingress, fake) = fake_ingress().await;

        ingress
            .send(
                "Batch/batch-1/run",
                &json!({ "action": "merge" }),
                Some("batch-1"),
            )
            .await
            .unwrap();

        let forward = fake.the_one_forward("/restate/send/Batch/batch-1/run");
        assert_eq!(forward.idempotency_key.as_deref(), Some("batch-1"));
        assert_eq!(
            serde_json::from_slice::<Value>(&forward.body).unwrap(),
            json!({ "action": "merge" })
        );
    }

    /// A call is answered with what the test put at its path, and a path nothing was put at
    /// answers as an object with no state does: with `null`, which is the handler's own
    /// `None` rather than a refusal — the read asks for `Option<T>` and gets it.
    #[tokio::test]
    async fn the_fake_ingress_answers_a_call_with_what_was_put_at_its_path() {
        let (ingress, fake) = fake_ingress().await;
        fake.answers("PullRequest/7%239/status", json!({ "syncing": true }));

        let answered: Option<Value> = ingress.call("PullRequest/7%239/status").await.unwrap();
        let absent: Option<Value> = ingress.call("PullRequest/7%2310/status").await.unwrap();

        assert_eq!(answered, Some(json!({ "syncing": true })));
        assert_eq!(absent, None);
    }

    #[tokio::test]
    async fn the_fake_ingress_refuses_the_paths_it_was_told_to() {
        let (ingress, fake) = fake_ingress().await;
        fake.refuses("Batch/batch-1/progress", StatusCode::NOT_FOUND);

        let answer: Option<Value> = ingress
            .call_optional("Batch/batch-1/progress")
            .await
            .unwrap();

        assert_eq!(
            answer, None,
            "a 404 from the ingress is an invocation it never had"
        );
    }
}
