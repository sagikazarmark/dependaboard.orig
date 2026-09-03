//! Bootstrap for the Restate service: resolves settings from the environment, binds every
//! component to the endpoint, and asks Restate to arm the installation scheduler.

mod bulk_action;
mod github;
mod handler;
mod ingress;
mod installation_sync;
mod pull_request;
mod repo_sync;
mod store;
#[cfg(test)]
mod test_support;

use std::{env, net::SocketAddr, time::Duration};

use dependaboard_github::{GithubClient, GithubConfig};
use dependaboard_store::{LibSqlPrStore, StoreConfig};
use restate_sdk::{filter::ReplayAwareFilter, prelude::*};
use tokio::{net::TcpListener, signal};
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    bulk_action::BulkAction,
    ingress::{SchedulerIngress, WebhookIngress},
    installation_sync::InstallationSync,
    pull_request::PullRequest,
    repo_sync::RepoSync,
};

const DEFAULT_DEBOUNCE_SECONDS: u64 = 20;
const DEFAULT_RECONCILE_SECONDS: u64 = 60 * 60;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Restate re-runs handler code while replaying a journal, so without the replay filter
    // every line a handler logged before it suspended would be logged again on resume.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
                .with_filter(ReplayAwareFilter),
        )
        .init();

    let store = LibSqlPrStore::connect(&StoreConfig::from_env()).await?;
    let github = GithubClient::new(GithubConfig::from_env()?)?;
    let debounce = Duration::from_secs(env_seconds(
        "SYNC_DEBOUNCE_SECONDS",
        DEFAULT_DEBOUNCE_SECONDS,
    ));
    let interval = Duration::from_secs(env_seconds(
        "RECONCILE_INTERVAL_SECONDS",
        DEFAULT_RECONCILE_SECONDS,
    ));
    let installation_id = github.installation_id();
    info!(
        installation_id,
        debounce_seconds = debounce.as_secs(),
        reconcile_interval_seconds = interval.as_secs(),
        "resolved service settings"
    );

    let pull_request = PullRequest {
        github: github.clone(),
        store: store.clone(),
        debounce,
    };
    let installation_sync = InstallationSync {
        github: github.clone(),
        store: store.clone(),
        interval,
    };
    let repo_sync = RepoSync { github, store };
    let endpoint = Endpoint::builder()
        .bind(pull_request)
        .bind(BulkAction)
        .bind(WebhookIngress { installation_id })
        .bind(SchedulerIngress { installation_id })
        .bind(installation_sync)
        .bind(repo_sync)
        .build();

    tokio::spawn(start_scheduler(installation_id));
    let address: SocketAddr = env::var("RESTATE_SERVICE_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:9080".to_owned())
        .parse()?;
    let listener = TcpListener::bind(address).await?;
    info!(%address, "starting Restate service endpoint");
    HttpServer::new(endpoint)
        .serve_with_cancel(listener, shutdown_signal())
        .await;
    info!("Restate service endpoint stopped");
    Ok(())
}

/// Resolves once the process is asked to stop, by Ctrl-C or by its supervisor.
///
/// The SDK only listens for SIGINT, but containers stop with SIGTERM; without this arm a
/// `docker stop` kills the process mid-step and every in-flight invocation has to be
/// replayed. Either signal starts the SDK's graceful drain: the listener closes, open
/// invocations get up to ten seconds to finish, and Restate retries whatever is left.
async fn shutdown_signal() {
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

async fn start_scheduler(installation_id: u64) {
    let ingress = env::var("RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
        .trim_end_matches('/')
        .to_owned();
    let api_key = env::var("RESTATE_AUTH_TOKEN")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            env::var("RESTATE_API_KEY")
                .ok()
                .filter(|value| !value.is_empty())
        });
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "could not build Restate scheduler client");
            return;
        }
    };
    let url = format!("{ingress}/restate/send/SchedulerIngress/start");
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let request = scheduler_start_request(&client, &url, api_key.as_deref());
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                info!(
                    installation_id,
                    "installation scheduler accepted by Restate"
                );
                return;
            }
            Ok(response) => {
                warn!(status = %response.status(), "Restate has not accepted scheduler startup yet")
            }
            Err(error) => warn!(%error, "Restate ingress is not ready for scheduler startup"),
        }
    }
}

fn scheduler_start_request(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut request = client.post(url);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    request
}

/// Reads a whole-seconds setting from the environment.
fn env_seconds(name: &str, default: u64) -> u64 {
    let raw = match env::var(name) {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        // Not UTF-8 cannot be a number either; surface it as the typo it is.
        Err(env::VarError::NotUnicode(raw)) => Some(raw.to_string_lossy().into_owned()),
    };
    resolve_seconds(name, raw.as_deref(), default)
}

/// Resolves a whole-seconds setting from its raw environment value.
///
/// A typo must not silently become the default: an unparsable value still falls back, but
/// says so and names the variable, so the operator learns at startup rather than from the
/// service's behaviour. Unset or empty means "use the default" and is not worth a line.
fn resolve_seconds(name: &str, raw: Option<&str>, default: u64) -> u64 {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return default;
    };
    match raw.parse::<u64>() {
        Ok(seconds) => seconds,
        Err(error) => {
            warn!(
                variable = name,
                value = raw,
                default,
                %error,
                "environment setting is not a whole number of seconds; using the default"
            );
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::captured_logs;

    #[test]
    fn scheduler_start_request_has_no_input_payload() {
        let request = scheduler_start_request(
            &reqwest::Client::new(),
            "http://127.0.0.1:8080/restate/send/SchedulerIngress/start",
            None,
        )
        .build()
        .expect("scheduler request should build");

        assert!(request.body().is_none());
        assert!(
            !request
                .headers()
                .contains_key(reqwest::header::CONTENT_TYPE)
        );
    }

    #[test]
    fn an_unparsable_seconds_setting_falls_back_and_warns_naming_the_variable() {
        let mut resolved = None;
        let logs = captured_logs(|| {
            resolved = Some(resolve_seconds("SYNC_DEBOUNCE_SECONDS", Some("20s"), 20));
        });

        assert_eq!(resolved, Some(20));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("SYNC_DEBOUNCE_SECONDS"), "{logs}");
        assert!(logs.contains("20s"), "{logs}");
    }

    #[test]
    fn configured_and_unset_seconds_settings_resolve_silently() {
        let mut resolved = Vec::new();
        let logs = captured_logs(|| {
            resolved.push(resolve_seconds(
                "RECONCILE_INTERVAL_SECONDS",
                Some("900"),
                3600,
            ));
            resolved.push(resolve_seconds("RECONCILE_INTERVAL_SECONDS", None, 3600));
            resolved.push(resolve_seconds(
                "RECONCILE_INTERVAL_SECONDS",
                Some(""),
                3600,
            ));
        });

        assert_eq!(resolved, vec![900, 3600, 3600]);
        assert!(logs.is_empty(), "{logs}");
    }
}
