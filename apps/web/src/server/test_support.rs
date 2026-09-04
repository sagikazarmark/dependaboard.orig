//! Shared fixtures for the server modules' unit tests.

use std::net::SocketAddr;

use crate::server::restate::RestateIngress;

/// A client for a Restate ingress stand-in listening at `address`.
pub(crate) fn ingress_at(address: SocketAddr) -> RestateIngress {
    RestateIngress {
        client: reqwest::Client::new(),
        base: format!("http://{address}"),
        token: None,
    }
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
