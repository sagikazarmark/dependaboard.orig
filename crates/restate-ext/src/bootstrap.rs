//! Arming durable work from a process's own startup.

use std::time::Duration;

use tracing::{info, warn};

use crate::ingress::IngressClient;

/// Sends the one-way invocation at `path` until Restate accepts it, waiting `retry_every`
/// between attempts.
///
/// A service that owns a perpetual chain — a scheduler whose tick arms the next tick — has
/// to be asked once for the chain to exist at all, and the only process that knows it should
/// be asked is the service itself. It cannot ask at startup and give up, because Restate's
/// ingress is usually not answering yet when the endpoint binds, and it must not ask twice
/// in a way that starts two chains: the handler it invokes is the one that has to be
/// idempotent, and a chain that is already armed answers that it is.
///
/// Spawn it; it returns once Restate has accepted, and keeps trying until then, so a caller
/// that wants to give up wraps it in a timeout.
pub async fn send_until_accepted(ingress: &IngressClient, path: &str, retry_every: Duration) {
    loop {
        match ingress.send_empty(path).await {
            Ok(()) => {
                info!(path, "Restate accepted the invocation");
                return;
            }
            Err(error) => {
                warn!(path, %error, "Restate has not accepted the invocation yet");
            }
        }
        tokio::time::sleep(retry_every).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{http::StatusCode, response::IntoResponse, routing::post};

    use super::*;
    use crate::test_support::{ingress_at, serve};

    /// The case it exists for: the ingress is not answering for the endpoint yet, and the
    /// arming keeps asking until it is.
    #[tokio::test]
    async fn an_invocation_is_sent_until_restate_accepts_it() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&attempts);
        let address = serve(axum::Router::new().route(
            "/restate/send/Scheduler/start",
            post(move || {
                let counted = Arc::clone(&counted);
                async move {
                    if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    (
                        StatusCode::ACCEPTED,
                        axum::Json(serde_json::json!({
                            "invocationId": "inv_1",
                            "status": "Accepted"
                        })),
                    )
                        .into_response()
                }
            }),
        ))
        .await;

        send_until_accepted(
            &ingress_at(address),
            "Scheduler/start",
            Duration::from_millis(10),
        )
        .await;

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "two refusals and the acceptance"
        );
    }
}
