//! Stopping an endpoint the way its supervisor asks it to.

use tokio::signal;
use tracing::{info, warn};

/// Resolves once the process is asked to stop, by Ctrl-C or by its supervisor.
///
/// Pass it to [`HttpServer::serve_with_cancel`](restate_sdk::prelude::HttpServer). The SDK
/// only listens for SIGINT, but containers stop with SIGTERM; without this arm a
/// `docker stop` kills the process mid-step and every in-flight invocation has to be
/// replayed. Either signal starts the SDK's graceful drain: the listener closes, open
/// invocations get up to ten seconds to finish, and Restate retries whatever is left.
pub async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = signal::ctrl_c().await {
            warn!(%error, "cannot listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(error) => {
                warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let signal = tokio::select! {
        () = interrupt => "SIGINT",
        () = terminate => "SIGTERM",
    };
    info!(signal, "shutdown requested; draining in-flight invocations");
}
