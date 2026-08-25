#![allow(non_snake_case)]

use std::collections::BTreeSet;

use dependaboard_core::{
    BatchProgress, BulkActionKind, CheckStatus, DashboardPage, Page, PrFilter, PrRecord, PrState,
    PrTarget, TargetProgressState, UpdateType, new_batch_id,
};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use {
    axum::{
        body::{Body, Bytes},
        extract::Extension,
        http::{HeaderMap, Request, StatusCode, header},
        middleware::{self, Next},
        response::{IntoResponse, Response},
        routing::post,
    },
    base64::{Engine as _, engine::general_purpose::STANDARD},
    dependaboard_core::{BulkRequest, DASHBOARD_SYNC_ACTION, PrKey, UserId, WebhookEvent},
    dependaboard_store::{LibSqlPrStore, PrStore, StoreConfig},
    dioxus::server::{DioxusRouterExt, ServeConfig},
    hmac::{Hmac, Mac},
    octocrab::models::webhook_events::{
        WebhookEvent as GithubWebhookEvent, payload::WebhookEventPayload,
    },
    serde::{Deserialize, Serialize, de::DeserializeOwned},
    serde_json::Value,
    sha2::Sha256,
};

#[cfg(feature = "server")]
static STORE: tokio::sync::OnceCell<LibSqlPrStore> = tokio::sync::OnceCell::const_new();

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
        .serve_dioxus_application(ServeConfig::new(), App)
        .layer(middleware::from_fn(require_dashboard_auth));
    let router = axum::Router::new()
        .route("/api/webhooks/github", post(github_webhook))
        .merge(dashboard);
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
    dioxus::launch(App);
}

#[server]
async fn load_dashboard(filter: PrFilter, page: Page) -> Result<DashboardPage, ServerFnError> {
    let store = store().await?;
    store
        .list_prs(&filter, page)
        .await
        .map_err(|error| ServerFnError::new(error.to_string()))
}

#[server(user: Extension<UserId>)]
async fn submit_batch(
    batch_id: String,
    action: BulkActionKind,
    targets: Vec<PrTarget>,
) -> Result<(), ServerFnError> {
    if !dependaboard_core::valid_batch_id(&batch_id) {
        return Err(ServerFnError::new("batch id must be a UUIDv7"));
    }
    if targets.is_empty() || targets.len() > dependaboard_core::MAX_BATCH_TARGETS {
        return Err(ServerFnError::new(format!(
            "batch must contain between 1 and {} targets",
            dependaboard_core::MAX_BATCH_TARGETS
        )));
    }
    let unique = targets.iter().map(PrTarget::key).collect::<BTreeSet<_>>();
    if unique.len() != targets.len() {
        return Err(ServerFnError::new("batch contains duplicate pull requests"));
    }
    let request = BulkRequest {
        action,
        targets,
        user_id: user.0,
    };
    restate_send(&format!("BulkAction/{batch_id}/run"), &request)
        .await
        .map_err(ServerFnError::new)
}

#[server]
async fn load_batch_progress(batch_id: String) -> Result<Option<BatchProgress>, ServerFnError> {
    if !dependaboard_core::valid_batch_id(&batch_id) {
        return Err(ServerFnError::new("batch id must be a UUIDv7"));
    }
    restate_call(&format!("BulkAction/{batch_id}/progress"))
        .await
        .map_err(ServerFnError::new)
}

#[server]
async fn load_pr_status(repository_id: u64, number: u64) -> Result<Option<PrState>, ServerFnError> {
    restate_call(&pr_status_path(repository_id, number))
        .await
        .map_err(ServerFnError::new)
}

#[server]
async fn load_pr_projection(
    repository_id: u64,
    number: u64,
) -> Result<Option<PrRecord>, ServerFnError> {
    store()
        .await?
        .get_pr(&PrKey::new(repository_id, number))
        .await
        .map_err(|error| ServerFnError::new(error.to_string()))
}

#[cfg(feature = "server")]
fn pr_status_path(repository_id: u64, number: u64) -> String {
    format!("PullRequest/{repository_id}%23{number}/status")
}

#[server]
async fn request_sync() -> Result<(), ServerFnError> {
    let installation_id = github_installation_id()?;
    let event = WebhookEvent {
        event: "installation_repositories".to_owned(),
        action: Some(DASHBOARD_SYNC_ACTION.to_owned()),
        installation_id: Some(installation_id),
        repository_id: None,
        owner: None,
        repo: None,
        number: None,
        sha: None,
        pull_requests: Vec::new(),
        sync_completion_id: None,
    };
    restate_send("WebhookIngress/dispatch", &event)
        .await
        .map_err(ServerFnError::new)
}

#[server]
async fn request_pr_sync(repository_id: u64, number: u64) -> Result<String, ServerFnError> {
    let installation_id = github_installation_id()?;
    let row = store()
        .await?
        .get_pr(&PrKey::new(repository_id, number))
        .await
        .map_err(|error| ServerFnError::new(error.to_string()))?
        .ok_or_else(|| ServerFnError::new("pull request is no longer in the dashboard"))?;
    if row.installation_id != installation_id {
        return Err(ServerFnError::new(
            "pull request does not belong to the configured installation",
        ));
    }
    let completion_id = new_batch_id();
    let event = manual_pr_sync_event(
        installation_id,
        row.repository_id,
        row.owner,
        row.repo,
        row.number,
        row.head_sha,
        completion_id.clone(),
    );
    restate_send("WebhookIngress/dispatch", &event)
        .await
        .map_err(ServerFnError::new)?;
    Ok(completion_id)
}

#[cfg(feature = "server")]
fn github_installation_id() -> Result<u64, ServerFnError> {
    std::env::var("GITHUB_INSTALLATION_ID")
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID is not configured"))?
        .parse::<u64>()
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID must be an integer"))
}

#[cfg(feature = "server")]
fn manual_pr_sync_event(
    installation_id: u64,
    repository_id: u64,
    owner: String,
    repo: String,
    number: u64,
    observed_sha: String,
    completion_id: String,
) -> WebhookEvent {
    WebhookEvent {
        event: "pull_request".to_owned(),
        action: Some(DASHBOARD_SYNC_ACTION.to_owned()),
        installation_id: Some(installation_id),
        repository_id: Some(repository_id),
        owner: Some(owner),
        repo: Some(repo),
        number: Some(number),
        sha: Some(observed_sha),
        pull_requests: Vec::new(),
        sync_completion_id: Some(completion_id),
    }
}

#[cfg(feature = "server")]
async fn store() -> Result<&'static LibSqlPrStore, ServerFnError> {
    STORE
        .get_or_try_init(|| async {
            LibSqlPrStore::connect(&StoreConfig::from_env())
                .await
                .map_err(|error| ServerFnError::new(error.to_string()))
        })
        .await
}

#[cfg(feature = "server")]
fn restate_client() -> Result<(reqwest::Client, String, Option<String>), String> {
    let base = std::env::var("RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned())
        .trim_end_matches('/')
        .to_owned();
    let token = std::env::var("RESTATE_AUTH_TOKEN")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("RESTATE_API_KEY")
                .ok()
                .filter(|value| !value.is_empty())
        });
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?;
    Ok((client, base, token))
}

#[cfg(feature = "server")]
async fn restate_send<T: Serialize + ?Sized>(path: &str, input: &T) -> Result<(), String> {
    let (client, base, token) = restate_client()?;
    let mut request = client
        .post(format!("{base}/restate/send/{path}"))
        .json(input);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let detail = response.text().await.unwrap_or_default();
    if status.as_u16() == 409
        && path.starts_with("BulkAction/")
        && detail.to_ascii_lowercase().contains("previously accepted")
    {
        return Ok(());
    }
    Err(format!("Restate returned {status}: {detail}"))
}

#[cfg(feature = "server")]
async fn restate_call<R>(path: &str) -> Result<R, String>
where
    R: DeserializeOwned,
{
    let (client, base, token) = restate_client()?;
    restate_call_with_client(&client, &base, token.as_deref(), path).await
}

#[cfg(feature = "server")]
async fn restate_call_with_client<R>(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    path: &str,
) -> Result<R, String>
where
    R: DeserializeOwned,
{
    let mut request = client.post(format!("{base}/restate/call/{path}"));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| format!("Restate returned an invalid response: {error}"))?;
    if !status.is_success() {
        return Err(format!("Restate returned {status}: {value}"));
    }
    let output = value.get("output").cloned().unwrap_or(value);
    serde_json::from_value(output).map_err(|error| error.to_string())
}

#[cfg(feature = "server")]
async fn github_webhook(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let Some(signature) = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
    else {
        return (StatusCode::UNAUTHORIZED, "missing webhook signature").into_response();
    };
    let secret = match std::env::var("GITHUB_WEBHOOK_SECRET") {
        Ok(secret) if !secret.is_empty() => secret,
        _ => {
            tracing::error!("GITHUB_WEBHOOK_SECRET is not configured");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "webhook is not configured",
            )
                .into_response();
        }
    };
    if !valid_signature(&secret, signature, &body) {
        return (StatusCode::UNAUTHORIZED, "invalid webhook signature").into_response();
    }
    let Some(event_name) = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing GitHub event name").into_response();
    };
    let event = match parse_webhook(event_name, &body) {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(%error, event = event_name, "GitHub webhook payload was rejected");
            return (StatusCode::BAD_REQUEST, "invalid GitHub webhook payload").into_response();
        }
    };
    match restate_send("WebhookIngress/dispatch", &event).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::error!(%error, event = event_name, "Restate rejected webhook");
            (StatusCode::BAD_GATEWAY, "could not enqueue webhook").into_response()
        }
    }
}

#[cfg(feature = "server")]
async fn require_dashboard_auth(mut request: Request<Body>, next: Next) -> Response {
    let username =
        std::env::var("DASHBOARD_USERNAME").unwrap_or_else(|_| "dependaboard".to_owned());
    let password = std::env::var("DASHBOARD_PASSWORD").unwrap_or_default();
    let authenticated_user = authenticated_dashboard_user(request.headers(), &username, &password);
    if let Some(user_id) = authenticated_user {
        request.extensions_mut().insert(user_id);
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"dependaboard\"")],
        "authentication required",
    )
        .into_response()
}

#[cfg(feature = "server")]
fn authenticated_dashboard_user(
    headers: &HeaderMap,
    username: &str,
    password: &str,
) -> Option<UserId> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| STANDARD.decode(value).ok())
        .and_then(|value| String::from_utf8(value).ok())
        .and_then(|credentials| {
            credentials
                .split_once(':')
                .map(|(user, pass)| (user.to_owned(), pass.to_owned()))
        })
        .filter(|(user, pass)| {
            constant_time_eq(user.as_bytes(), username.as_bytes())
                && constant_time_eq(pass.as_bytes(), password.as_bytes())
        })
        .map(|(user, _)| UserId::new(user))
}

#[cfg(feature = "server")]
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

#[cfg(feature = "server")]
fn valid_signature(secret: &str, signature: &str, body: &[u8]) -> bool {
    let Some(encoded) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(encoded) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(feature = "server")]
fn parse_webhook(event_name: &str, body: &[u8]) -> Result<WebhookEvent, String> {
    let event = GithubWebhookEvent::try_from_header_and_body(event_name, body)
        .map_err(|error| error.to_string())?;
    normalize_webhook(event_name, &event)
}

#[cfg(feature = "server")]
fn normalize_webhook(event_name: &str, event: &GithubWebhookEvent) -> Result<WebhookEvent, String> {
    let (action, number, sha, pull_requests) = match &event.specific {
        WebhookEventPayload::Installation(payload) => {
            (Some(action_name(&payload.action)?), None, None, Vec::new())
        }
        WebhookEventPayload::InstallationRepositories(payload) => {
            (Some(action_name(&payload.action)?), None, None, Vec::new())
        }
        WebhookEventPayload::PullRequest(payload) => (
            Some(action_name(&payload.action)?),
            Some(payload.pull_request.number),
            Some(payload.pull_request.head.sha.clone()),
            Vec::new(),
        ),
        WebhookEventPayload::CheckRun(payload) => {
            let routing: CheckRouting = serde_json::from_value(payload.check_run.clone())
                .map_err(|error| error.to_string())?;
            (
                Some(action_name(&payload.action)?),
                None,
                Some(routing.head_sha),
                routing
                    .pull_requests
                    .into_iter()
                    .map(|pull| pull.number)
                    .collect(),
            )
        }
        WebhookEventPayload::CheckSuite(payload) => {
            let routing: CheckRouting = serde_json::from_value(payload.check_suite.clone())
                .map_err(|error| error.to_string())?;
            (
                Some(action_name(&payload.action)?),
                None,
                Some(routing.head_sha),
                routing
                    .pull_requests
                    .into_iter()
                    .map(|pull| pull.number)
                    .collect(),
            )
        }
        WebhookEventPayload::Status(payload) => (None, None, Some(payload.sha.clone()), Vec::new()),
        _ => (None, None, None, Vec::new()),
    };
    let installation_id = event
        .installation
        .as_ref()
        .map(|installation| installation.id().0);
    let repository_id = event.repository.as_ref().map(|repository| repository.id.0);
    let owner = event
        .repository
        .as_ref()
        .and_then(|repository| repository.owner.as_ref())
        .map(|owner| owner.login.clone());
    let repo = event
        .repository
        .as_ref()
        .map(|repository| repository.name.clone());
    match &event.specific {
        WebhookEventPayload::Installation(_) | WebhookEventPayload::InstallationRepositories(_)
            if installation_id.is_none() =>
        {
            return Err("GitHub installation webhook is missing an installation id".to_owned());
        }
        WebhookEventPayload::PullRequest(_)
        | WebhookEventPayload::CheckRun(_)
        | WebhookEventPayload::CheckSuite(_)
        | WebhookEventPayload::Status(_)
            if repository_id.is_none() || owner.is_none() || repo.is_none() =>
        {
            return Err("GitHub repository webhook is missing routing fields".to_owned());
        }
        _ => {}
    }
    Ok(WebhookEvent {
        event: event_name.to_owned(),
        action,
        installation_id,
        repository_id,
        owner,
        repo,
        number,
        sha,
        pull_requests,
        sync_completion_id: None,
    })
}

#[cfg(feature = "server")]
fn action_name(action: &impl Serialize) -> Result<String, String> {
    serde_json::to_value(action)
        .map_err(|error| error.to_string())?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| "GitHub webhook action was not a string".to_owned())
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct CheckRouting {
    head_sha: String,
    #[serde(default)]
    pull_requests: Vec<CheckPullRequest>,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct CheckPullRequest {
    number: u64,
}

fn App() -> Element {
    let mut dark = use_signal(|| true);
    let mut aside_open = use_signal(|| true);
    let mut filter = use_signal(PrFilter::default);
    let mut cursor = use_signal(|| None::<String>);
    let mut refresh = use_signal(|| 0_u64);
    let mut selected = use_signal(BTreeSet::<String>::new);
    let mut detail = use_signal(|| None::<PrRecord>);
    let mut confirm = use_signal(|| None::<BulkActionKind>);
    let mut active_batch = use_signal(|| None::<BatchProgress>);
    let mut progress_open = use_signal(|| false);
    let mut toast = use_signal(|| None::<String>);

    use_effect(move || {
        let _ = filter();
        let _ = cursor();
        selected.write().clear();
    });

    let mut dashboard = use_resource(move || {
        let filter = filter();
        let page = Page {
            limit: 50,
            after: cursor(),
        };
        let _ = refresh();
        async move { load_dashboard(filter, page).await }
    });

    let page = dashboard
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .cloned();
    let load_error = dashboard
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .map(ToString::to_string);
    let rows = page
        .as_ref()
        .map(|page| page.rows.clone())
        .unwrap_or_default();
    let total = page.as_ref().map(|page| page.total).unwrap_or_default();
    let selected_count = selected.read().len();
    let theme = if dark() {
        "dependaboard-dark"
    } else {
        "dependaboard-light"
    };
    let active_filter_count = filter_count(&filter());

    rsx! {
        document::Link { rel: "preconnect", href: "https://fonts.googleapis.com" }
        document::Link { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "anonymous" }
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap"
        }
        document::Stylesheet { href: asset!("/assets/main.css") }

        div { class: "app-shell", "data-theme": theme,
            header { class: "topbar",
                div { class: "brand",
                    button {
                        class: "icon-button",
                        title: "Show or hide filters",
                        onclick: move |_| aside_open.toggle(),
                        "="
                    }
                    span { class: "brand-mark" }
                    strong { "dependabot" }
                    span { class: "muted mono account-label", "/ installations" }
                }
                div { class: "view-switch",
                    button {
                        class: if !filter().needs_attention { "active" } else { "" },
                        onclick: move |_| {
                            filter.write().needs_attention = false;
                            cursor.set(None);
                        },
                        "All open"
                    }
                    button {
                        class: if filter().needs_attention { "active" } else { "" },
                        onclick: move |_| {
                            filter.write().needs_attention = true;
                            cursor.set(None);
                        },
                        "Needs attention"
                    }
                }
                div { class: "topbar-spacer" }
                button {
                    class: "btn btn-ghost btn-xs theme-button",
                    title: "Toggle color theme",
                    onclick: move |_| dark.toggle(),
                    if dark() { "light" } else { "dark" }
                }
                button {
                    class: "btn btn-sm sync-button",
                    onclick: move |_| {
                        spawn(async move {
                            if let Err(error) = request_sync().await {
                                toast.set(Some(format!("Sync failed: {error}")));
                            } else {
                                toast.set(Some("Reconciliation queued".to_owned()));
                                wait_one_second().await;
                                refresh += 1;
                                dashboard.restart();
                            }
                        });
                    },
                    span { class: "sync-glyph", "+" }
                    "Sync"
                }
            }

            div { class: "workspace",
                aside {
                    class: if aside_open() { "sidebar" } else { "sidebar sidebar-closed" },
                    div { class: "search-wrap",
                        span { "/" }
                        input {
                            class: "input input-sm",
                            value: filter().query.unwrap_or_default(),
                            placeholder: "dependency, repo, title...",
                            oninput: move |event| {
                                let value = event.value();
                                filter.write().query = (!value.trim().is_empty()).then_some(value);
                                cursor.set(None);
                            }
                        }
                    }
                    div { class: "filter-meta",
                        span { class: "mono muted", "{active_filter_count} active" }
                        button {
                            disabled: active_filter_count == 0,
                            onclick: move |_| {
                                filter.set(PrFilter::default());
                                cursor.set(None);
                                selected.write().clear();
                            },
                            "clear"
                        }
                    }

                    FilterSection { title: "Check rollup" }
                    div { class: "facet-list",
                        for status in CheckStatus::ALL {
                            FacetButton {
                                key: "check-{status}",
                                label: status_label(status),
                                count: facet_count(page.as_ref(), "check", &status.to_string()),
                                active: filter().check_statuses.contains(&status),
                                tone: status_class(status),
                                onclick: move |_| {
                                    toggle_value(&mut filter.write().check_statuses, status);
                                    cursor.set(None);
                                }
                            }
                        }
                    }

                    FilterSection { title: "Update type" }
                    div { class: "facet-list",
                        for update_type in UpdateType::ALL {
                            FacetButton {
                                key: "type-{update_type}",
                                label: update_type.to_string(),
                                count: facet_count(page.as_ref(), "type", &update_type.to_string()),
                                active: filter().update_types.contains(&update_type),
                                tone: update_class(update_type),
                                onclick: move |_| {
                                    toggle_value(&mut filter.write().update_types, update_type);
                                    cursor.set(None);
                                }
                            }
                        }
                    }

                    FilterSection { title: "Labels" }
                    div { class: "label-facets",
                        if let Some(page) = &page {
                            for (label, count) in page.facets.labels.iter().take(8) {
                                {
                                    let label_value = label.clone();
                                    let active = filter().labels.contains(label);
                                    rsx! {
                                        button {
                                            class: if active { "label-filter active" } else { "label-filter" },
                                            onclick: move |_| {
                                                toggle_value(&mut filter.write().labels, label_value.clone());
                                                cursor.set(None);
                                            },
                                            "{label} " span { "{count}" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    FilterSection { title: "Accounts & repositories" }
                    div { class: "repo-list",
                        if let Some(page) = &page {
                            for repository in &page.repositories {
                                {
                                    let full_name = format!("{}/{}", repository.owner, repository.repo);
                                    let selected_repo = filter().repos.contains(&full_name);
                                    let repo_value = full_name.clone();
                                    rsx! {
                                        button {
                                            class: if selected_repo { "repo-filter active" } else { "repo-filter" },
                                            onclick: move |_| {
                                                toggle_value(&mut filter.write().repos, repo_value.clone());
                                                cursor.set(None);
                                            },
                                            span { class: "selection-box", if selected_repo { "x" } }
                                            span { class: "repo-owner", "{repository.owner}/" }
                                            span { "{repository.repo}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                main { class: "content",
                    div { class: "resultbar",
                        span { class: "mono", "{total} pull requests" }
                        button {
                            disabled: rows.is_empty(),
                            onclick: {
                                let rows = rows.clone();
                                move |_| {
                                    let all_selected = rows.iter().all(|row| selected.read().contains(&row.id));
                                    if all_selected {
                                        for row in &rows { selected.write().remove(&row.id); }
                                    } else {
                                        for row in &rows { selected.write().insert(row.id.clone()); }
                                    }
                                }
                            },
                            if rows.iter().all(|row| selected.read().contains(&row.id)) && !rows.is_empty() {
                                "clear visible"
                            } else {
                                "select visible"
                            }
                        }
                        span { class: "result-spacer" }
                        span { class: "muted mono desktop-only", "updated recently first" }
                    }
                    if active_filter_count > 0 {
                        ActiveFilters { filter, cursor }
                    }
                    div { class: "table-scroll",
                        div { class: "pr-grid table-head",
                            span {}
                            span { "PR" }
                            span { "Dependency" }
                            span { "Repository" }
                            span { "Checks" }
                            span { "Labels" }
                            span { class: "right", "Updated" }
                        }
                        if let Some(error) = load_error {
                            div { class: "empty-state error-state",
                                strong { "The read model could not be loaded" }
                                code { "{error}" }
                                button { class: "btn btn-sm", onclick: move |_| dashboard.restart(), "Retry" }
                            }
                        } else if page.is_none() {
                            div { class: "loading-state",
                                span { class: "loading loading-spinner loading-sm" }
                                "Reading the projection"
                            }
                        } else if rows.is_empty() {
                            div { class: "empty-state",
                                span { class: "empty-mark" }
                                strong { "No open Dependabot pull requests" }
                                p { "Try clearing filters or queue a reconciliation sweep." }
                            }
                        } else {
                            for row in &rows {
                                PrRow {
                                    key: "{row.id}",
                                    row: row.clone(),
                                    checked: selected.read().contains(&row.id),
                                    oncheck: move |id: String| {
                                        if !selected.write().insert(id.clone()) {
                                            selected.write().remove(&id);
                                        }
                                    },
                                    onopen: move |row: PrRecord| detail.set(Some(row))
                                }
                            }
                        }
                        if let Some(next) = page.as_ref().and_then(|page| page.next_cursor.clone()) {
                            div { class: "load-more",
                                button {
                                    class: "btn btn-sm btn-ghost",
                                    onclick: move |_| {
                                        selected.write().clear();
                                        cursor.set(Some(next.clone()));
                                    },
                                    "Load next 50"
                                }
                            }
                        }
                    }
                }
            }

            footer { class: "statusbar",
                span { class: "status-dot" }
                span { "projection online" }
                span { class: "status-spacer" }
                if let Some(synced) = page.as_ref().and_then(|page| page.last_synced_at) {
                    span { "last event {relative_time(synced)}" }
                } else {
                    span { "waiting for first reconciliation" }
                }
            }

            if selected_count > 0 {
                div { class: "action-bar",
                    strong { class: "mono", "{selected_count} selected" }
                    button { class: "action-link", onclick: move |_| selected.write().clear(), "clear" }
                    span { class: "action-divider" }
                    button { class: "btn btn-sm rebase-button", onclick: move |_| confirm.set(Some(BulkActionKind::Rebase)), "Request rebase" }
                    button { class: "btn btn-sm merge-button", onclick: move |_| confirm.set(Some(BulkActionKind::Merge)), "Merge selected" }
                }
            }

            if let Some(row) = detail() {
                DetailDrawer {
                    row,
                    onclose: move |_| detail.set(None),
                    onsync: move |result: Result<Option<PrRecord>, String>| match result {
                        Ok(Some(row)) => {
                            detail.set(Some(row));
                            toast.set(Some("Pull request synced".to_owned()));
                            refresh += 1;
                            dashboard.restart();
                        }
                        Ok(None) => {
                            detail.set(None);
                            toast.set(Some("Pull request is no longer open".to_owned()));
                            refresh += 1;
                            dashboard.restart();
                        }
                        Err(error) => toast.set(Some(error)),
                    }
                }
            }

            if let Some(progress) = active_batch() {
                button {
                    class: "progress-pill",
                    onclick: move |_| progress_open.toggle(),
                    span { class: if progress.completed { "progress-live complete" } else { "progress-live" } }
                    "{progress.action}: {progress.succeeded + progress.rejected}/{progress.targets.len()}"
                }
                if progress_open() {
                    ProgressDrawer { progress: progress.clone(), onclose: move |_| progress_open.set(false) }
                }
            }

            if let Some(action) = confirm() {
                ConfirmModal {
                    action,
                    count: selected_count,
                    oncancel: move |_| confirm.set(None),
                    onconfirm: {
                        let rows = rows.clone();
                        move |_| {
                            let targets = rows
                                .iter()
                                .filter(|row| selected.read().contains(&row.id))
                                .map(pr_target)
                                .collect::<Vec<_>>();
                            let batch_id = new_batch_id();
                            active_batch.set(Some(BatchProgress::queued(
                                &batch_id,
                                action,
                                &targets,
                            )));
                            progress_open.set(true);
                            confirm.set(None);
                            selected.write().clear();
                            spawn(async move {
                                loop {
                                    match submit_batch(batch_id.clone(), action, targets.clone()).await {
                                        Ok(()) => break,
                                        Err(error) => {
                                            if let Ok(Some(progress)) = load_batch_progress(batch_id.clone()).await {
                                                active_batch.set(Some(progress));
                                                break;
                                            }
                                            toast.set(Some(format!("Batch submission interrupted; retrying: {error}")));
                                            wait_one_second().await;
                                        }
                                    }
                                }
                                loop {
                                    wait_one_second().await;
                                    match load_batch_progress(batch_id.clone()).await {
                                        Ok(Some(progress)) => {
                                            let completed = progress.completed;
                                            let failed = progress.failure.clone();
                                            active_batch.set(Some(progress));
                                            if completed {
                                                toast.set(Some(failed.map_or_else(
                                                    || "Batch complete".to_owned(),
                                                    |failure| format!("Batch failed: {failure}"),
                                                )));
                                                refresh += 1;
                                                dashboard.restart();
                                                break;
                                            }
                                        }
                                        Ok(None) => {}
                                        Err(error) => {
                                            toast.set(Some(format!("Progress interrupted; retrying: {error}")));
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
            }

            if let Some(message) = toast() {
                div { class: "toast toast-end toast-bottom",
                    div { class: "alert toast-message",
                        span { "{message}" }
                        button { onclick: move |_| toast.set(None), "x" }
                    }
                }
            }
        }
    }
}

#[component]
fn FilterSection(title: &'static str) -> Element {
    rsx! { h2 { class: "filter-title", "{title}" } }
}

#[component]
fn FacetButton(
    label: String,
    count: u64,
    active: bool,
    tone: &'static str,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        button {
            class: if active { "facet active" } else { "facet" },
            onclick,
            span { class: "facet-dot {tone}" }
            span { "{label}" }
            code { "{count}" }
        }
    }
}

#[component]
fn ActiveFilters(mut filter: Signal<PrFilter>, mut cursor: Signal<Option<String>>) -> Element {
    let value = filter();
    rsx! {
        div { class: "active-filters",
            if value.needs_attention {
                FilterChip { kind: "view", label: "needs attention", onclick: move |_| { filter.write().needs_attention = false; cursor.set(None); } }
            }
            for status in value.check_statuses {
                FilterChip { key: "chip-check-{status}", kind: "checks", label: status.to_string(), onclick: move |_| { toggle_value(&mut filter.write().check_statuses, status); cursor.set(None); } }
            }
            for update_type in value.update_types {
                FilterChip { key: "chip-type-{update_type}", kind: "type", label: update_type.to_string(), onclick: move |_| { toggle_value(&mut filter.write().update_types, update_type); cursor.set(None); } }
            }
            for repo in value.repos {
                {
                    let repo_value = repo.clone();
                    rsx! { FilterChip { key: "chip-repo-{repo}", kind: "repo", label: repo, onclick: move |_| { toggle_value(&mut filter.write().repos, repo_value.clone()); cursor.set(None); } } }
                }
            }
            for label in value.labels {
                {
                    let label_value = label.clone();
                    rsx! { FilterChip { key: "chip-label-{label}", kind: "label", label, onclick: move |_| { toggle_value(&mut filter.write().labels, label_value.clone()); cursor.set(None); } } }
                }
            }
            button { class: "clear-all", onclick: move |_| { filter.set(PrFilter::default()); cursor.set(None); }, "clear all" }
        }
    }
}

#[component]
fn FilterChip(kind: &'static str, label: String, onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button { class: "filter-chip", onclick,
            span { "{kind}" }
            "{label}"
            b { "x" }
        }
    }
}

#[component]
fn PrRow(
    row: PrRecord,
    checked: bool,
    oncheck: EventHandler<String>,
    onopen: EventHandler<PrRecord>,
) -> Element {
    let id = row.id.clone();
    let opened = row.clone();
    let dependency = row
        .dependency
        .clone()
        .unwrap_or_else(|| format!("{} dependencies", row.dependencies.len()));
    let versions = match (&row.from_version, &row.to_version) {
        (Some(from), Some(to)) => format!("{from} -> {to}"),
        _ => row.title.clone(),
    };
    let stale = unix_seconds().saturating_sub(row.synced_at) > 45 * 60;
    let row_class = match (checked, stale) {
        (true, true) => "pr-grid pr-row selected stale",
        (true, false) => "pr-grid pr-row selected",
        (false, true) => "pr-grid pr-row stale",
        (false, false) => "pr-grid pr-row",
    };
    rsx! {
        div { class: row_class, onclick: move |_| onopen.call(opened.clone()),
            button {
                class: if checked { "selection-box checked" } else { "selection-box" },
                onclick: move |event| { event.stop_propagation(); oncheck.call(id.clone()); },
                if checked { "x" }
            }
            code { class: "pr-number", "#{row.number}" }
            div { class: "dependency-cell",
                div {
                    strong { "{dependency}" }
                    span { "{versions}" }
                    span { class: "update-chip {update_class(row.update_type)}", "{row.update_type}" }
                }
                small { "{row.title}" }
            }
            code { class: "repository-cell", span { "{row.owner}/" } "{row.repo}" }
            div { class: "check-cell",
                span { class: "check-dot {status_class(row.check_status)}" }
                span { "{status_label(row.check_status)}" }
            }
            div { class: "row-labels",
                for label in row.labels.iter().take(2) { span { "{label}" } }
                if row.labels.len() > 2 { span { "+{row.labels.len() - 2}" } }
            }
            time { class: "right mono", "{relative_time(row.updated_at)}" }
        }
    }
}

#[component]
fn DetailDrawer(
    row: PrRecord,
    onclose: EventHandler<MouseEvent>,
    onsync: EventHandler<Result<Option<PrRecord>, String>>,
) -> Element {
    let stale = unix_seconds().saturating_sub(row.synced_at) > 45 * 60;
    let mut syncing = use_signal(|| false);
    let mut sync_queued = use_signal(|| false);
    let mut status = use_resource({
        let repository_id = row.repository_id;
        let number = row.number;
        move || load_pr_status(repository_id, number)
    });
    let durable_state = status
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .cloned()
        .flatten();
    let status_error = status
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().err())
        .map(ToString::to_string);
    let sync_repository_id = row.repository_id;
    let sync_number = row.number;
    rsx! {
        div { class: "drawer-scrim", onclick: move |event| onclose.call(event),
            section { class: "side-drawer detail-drawer", onclick: move |event| event.stop_propagation(),
                div { class: "drawer-head",
                    div { span { class: "eyebrow", "Pull request" } h2 { "{row.owner}/{row.repo} #{row.number}" } }
                    button { class: "close-button", onclick: move |event| onclose.call(event), "x" }
                }
                div { class: "drawer-body",
                    h3 { "{row.title}" }
                    div { class: "drawer-badges",
                        span { class: "update-chip {update_class(row.update_type)}", "{row.update_type}" }
                        span { class: "status-badge", span { class: "check-dot {status_class(row.check_status)}" } "{status_label(row.check_status)}" }
                        if let Some(mergeable) = &row.mergeable { span { class: "status-badge", "{mergeable}" } }
                        if stale { span { class: "status-badge stale-badge", "projection stale" } }
                    }
                    dl { class: "detail-list",
                        dt { "Head SHA" } dd { code { "{row.head_sha}" } }
                        dt { "Last updated" } dd { "{relative_time(row.updated_at)}" }
                        dt { "Projected" } dd { "{relative_time(row.synced_at)}" }
                    }
                    h4 { "Dependencies" }
                    div { class: "dependency-list",
                        for dependency in &row.dependencies {
                            div {
                                strong { "{dependency.name}" }
                                code { "{version_label(dependency.from_version.as_deref(), dependency.to_version.as_deref())}" }
                                span { class: "update-chip {update_class(dependency.update_type)}", "{dependency.update_type}" }
                            }
                        }
                    }
                    h4 { "Labels" }
                    div { class: "drawer-labels",
                        for label in &row.labels { span { "{label}" } }
                    }
                    h4 { "Durable state" }
                    if let Some(error) = status_error {
                        p { class: "batch-failure", "Could not load activity: {error}" }
                    } else if let Some(state) = durable_state {
                        dl { class: "detail-list",
                            dt { "Last canonical sync" }
                            dd {
                                if let Some(last_synced_at) = state.last_synced_at {
                                    "{relative_time(last_synced_at)}"
                                } else {
                                    "not yet"
                                }
                            }
                            dt { "Debounced sync" }
                            dd { if state.sync_pending { "pending" } else { "idle" } }
                        }
                        div { class: "dependency-list",
                            if state.history.is_empty() {
                                div { "No durable activity recorded." }
                            } else {
                                for entry in state.history.iter().rev() {
                                    div {
                                        strong { "{entry.action}" }
                                        code { "{relative_time(entry.at)}" }
                                        span { "{entry.detail}" }
                                    }
                                }
                            }
                        }
                    } else {
                        div { class: "loading-state",
                            span { class: "loading loading-spinner loading-sm" }
                            "Reading durable activity"
                        }
                    }
                }
                div { class: "drawer-actions",
                    button {
                        class: "drawer-action drawer-sync",
                        disabled: syncing() || sync_queued(),
                        onclick: move |_| {
                            syncing.set(true);
                            spawn(async move {
                                match request_pr_sync(sync_repository_id, sync_number).await {
                                    Ok(completion_id) => {
                                        syncing.set(false);
                                        sync_queued.set(true);
                                        match wait_for_pr_sync_completion(
                                            sync_repository_id,
                                            sync_number,
                                            completion_id,
                                        ).await {
                                            Ok(row) => {
                                                sync_queued.set(false);
                                                status.restart();
                                                onsync.call(Ok(row));
                                            }
                                            Err(error) => {
                                                sync_queued.set(false);
                                                onsync.call(Err(format!(
                                                    "Sync was queued, but completion could not be confirmed: {error}"
                                                )));
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        syncing.set(false);
                                        onsync.call(Err(format!("Could not queue sync: {error}")));
                                    }
                                }
                            });
                        },
                        if syncing() {
                            "Queueing sync..."
                        } else if sync_queued() {
                            "Syncing..."
                        } else {
                            "Sync PR"
                        }
                    }
                    a { class: "drawer-action drawer-link", href: row.html_url, target: "_blank", rel: "noreferrer", "Open on GitHub" }
                }
            }
        }
    }
}

#[component]
fn ConfirmModal(
    action: BulkActionKind,
    count: usize,
    oncancel: EventHandler<MouseEvent>,
    onconfirm: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        div { class: "modal modal-open",
            div { class: "modal-box confirm-box",
                span { class: "eyebrow", "Durable bulk action" }
                h2 { "{action} {count} pull requests?" }
                p { "The selected head SHAs are captured now. Moved or ineligible pull requests will be rejected, not silently retried against new code." }
                if action == BulkActionKind::Merge {
                    div { class: "notice", "Uses the globally configured merge method." }
                } else {
                    div { class: "notice", "Rebase is requested by an idempotent @dependabot comment using the configured user token." }
                }
                div { class: "modal-action",
                    button { class: "btn btn-ghost btn-sm", onclick: move |event| oncancel.call(event), "Cancel" }
                    button { class: "btn btn-sm confirm-button", onclick: move |event| onconfirm.call(event), "Queue {action}" }
                }
            }
        }
    }
}

#[component]
fn ProgressDrawer(progress: BatchProgress, onclose: EventHandler<MouseEvent>) -> Element {
    let completed = progress.succeeded + progress.rejected;
    let total = progress.targets.len();
    let percentage = if total == 0 {
        100
    } else {
        completed * 100 / total as u64
    };
    rsx! {
        div { class: "drawer-scrim", onclick: move |event| onclose.call(event),
            section { class: "side-drawer progress-drawer", onclick: move |event| event.stop_propagation(),
                div { class: "drawer-head",
                    div { span { class: "eyebrow", "Batch {progress.batch_id}" } h2 { "{progress.action} progress" } }
                    button { class: "close-button", onclick: move |event| onclose.call(event), "x" }
                }
                div { class: "progress-summary",
                    strong { "{completed}/{total}" }
                    span { "{progress.succeeded} succeeded, {progress.rejected} rejected" }
                    progress { class: "progress progress-primary", max: "100", value: "{percentage}" }
                    if let Some(failure) = &progress.failure {
                        p { class: "batch-failure", "{failure}" }
                    }
                }
                div { class: "progress-list",
                    for item in &progress.targets {
                        div { class: "progress-row",
                            span { class: "progress-state {progress_class(&item.state)}" }
                            div {
                                if let Some(html_url) = progress_target_url(&item.target) {
                                    a {
                                        class: "progress-pr-link",
                                        href: html_url,
                                        target: "_blank",
                                        rel: "noreferrer",
                                        strong { "{item.target.owner}/{item.target.repo} #{item.target.number}" }
                                    }
                                } else {
                                    strong { "{item.target.owner}/{item.target.repo} #{item.target.number}" }
                                }
                                small { "{progress_detail(&item.state)}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn pr_target(row: &PrRecord) -> PrTarget {
    PrTarget {
        repository_id: row.repository_id,
        owner: row.owner.clone(),
        repo: row.repo.clone(),
        number: row.number,
        expected_sha: row.head_sha.clone(),
        title: row.title.clone(),
        html_url: row.html_url.clone(),
    }
}

fn progress_target_url(target: &PrTarget) -> Option<&str> {
    (!target.html_url.is_empty()).then_some(target.html_url.as_str())
}

fn filter_count(filter: &PrFilter) -> usize {
    usize::from(filter.query.is_some())
        + usize::from(filter.owner.is_some())
        + filter.repos.len()
        + filter.update_types.len()
        + filter.check_statuses.len()
        + filter.labels.len()
        + usize::from(filter.dependency.is_some())
        + usize::from(filter.needs_attention)
}

fn toggle_value<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if let Some(index) = values.iter().position(|candidate| candidate == &value) {
        values.remove(index);
    } else {
        values.push(value);
    }
}

fn facet_count(page: Option<&DashboardPage>, facet: &str, key: &str) -> u64 {
    page.and_then(|page| match facet {
        "check" => page.facets.checks.get(key),
        "type" => page.facets.update_types.get(key),
        _ => None,
    })
    .copied()
    .unwrap_or_default()
}

fn status_label(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Success => "Passing",
        CheckStatus::Failure => "Failing",
        CheckStatus::Pending => "Pending",
        CheckStatus::None => "No checks",
    }
}

fn status_class(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Success => "status-success",
        CheckStatus::Failure => "status-failure",
        CheckStatus::Pending => "status-pending",
        CheckStatus::None => "status-none",
    }
}

fn update_class(update_type: UpdateType) -> &'static str {
    match update_type {
        UpdateType::Major => "type-major",
        UpdateType::Minor => "type-minor",
        UpdateType::Patch => "type-patch",
        UpdateType::Unknown => "type-unknown",
    }
}

fn progress_class(state: &TargetProgressState) -> &'static str {
    match state {
        TargetProgressState::Queued => "state-queued",
        TargetProgressState::Running => "state-running",
        TargetProgressState::Succeeded { .. } => "state-succeeded",
        TargetProgressState::Rejected { .. } => "state-rejected",
        TargetProgressState::Failed { .. } => "state-failed",
    }
}

fn progress_detail(state: &TargetProgressState) -> String {
    match state {
        TargetProgressState::Queued => "queued".to_owned(),
        TargetProgressState::Running => "running".to_owned(),
        TargetProgressState::Succeeded { detail } => detail.clone(),
        TargetProgressState::Rejected { reason } => reason.to_string(),
        TargetProgressState::Failed { detail } => detail.clone(),
    }
}

fn version_label(from: Option<&str>, to: Option<&str>) -> String {
    match (from, to) {
        (Some(from), Some(to)) => format!("{from} -> {to}"),
        _ => "group update".to_owned(),
    }
}

fn relative_time(timestamp: u64) -> String {
    let now = unix_seconds();
    let seconds = now.saturating_sub(timestamp);
    match seconds {
        0..=59 => "now".to_owned(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        86_400..=604_799 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

fn unix_seconds() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn wait_one_second() {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(1_000).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

async fn wait_for_pr_sync_completion(
    repository_id: u64,
    number: u64,
    completion_id: String,
) -> Result<Option<PrRecord>, String> {
    let mut last_error = None;
    for _ in 0..60 {
        wait_one_second().await;
        match load_pr_status(repository_id, number).await {
            Ok(state) if sync_id_completed(state.as_ref(), &completion_id) => {
                match load_pr_projection(repository_id, number).await {
                    Ok(row) => return Ok(row),
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
            Ok(_) => match load_pr_projection(repository_id, number).await {
                Ok(None) => return Ok(None),
                Ok(Some(_)) => last_error = None,
                Err(error) => last_error = Some(error.to_string()),
            },
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(last_error.unwrap_or_else(|| "the sync did not complete within 60 seconds".to_owned()))
}

fn sync_id_completed(state: Option<&PrState>, completion_id: &str) -> bool {
    state.is_some_and(|state| {
        state
            .completed_sync_ids
            .iter()
            .any(|completed| completed == completion_id)
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn relative_time_uses_a_supported_browser_clock() {
        let now = unix_seconds();
        assert!(now > 1_577_836_800);
        assert_eq!(relative_time(now), "now");
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    #[test]
    fn batch_progress_uses_the_canonical_pull_request_url() {
        let mut target = PrTarget {
            repository_id: 7,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            expected_sha: "abc123".to_owned(),
            title: "Bump serde".to_owned(),
            html_url: "https://github.example/acme/api/pull/9".to_owned(),
        };
        assert_eq!(
            progress_target_url(&target),
            Some("https://github.example/acme/api/pull/9")
        );

        target.html_url.clear();
        assert_eq!(progress_target_url(&target), None);
    }

    #[tokio::test]
    async fn empty_input_restate_call_has_no_body_or_content_type() {
        async fn restate_ingress(headers: HeaderMap, body: Bytes) -> impl IntoResponse {
            if headers.contains_key(header::CONTENT_TYPE) || !body.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "code": 400,
                        "message": "input validation error: Expected body and content-type to be empty, but wasn't",
                        "source": "ingress"
                    })),
                );
            }
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({ "output": null })),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/restate/call/PullRequest/7%239/status",
                    post(restate_ingress),
                ),
            )
            .await
            .unwrap();
        });

        let result = restate_call_with_client::<Option<PrState>>(
            &reqwest::Client::new(),
            &format!("http://{address}"),
            None,
            "PullRequest/7%239/status",
        )
        .await;

        server.abort();
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn webhook_signature_is_verified_in_constant_time() {
        let body = br#"{"action":"opened"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        assert!(valid_signature("secret", &signature, body));
        assert!(!valid_signature("wrong", &signature, body));
        assert!(!valid_signature("secret", "sha1=abcd", body));
    }

    #[test]
    fn credential_comparison_handles_different_lengths() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrex"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }

    #[test]
    fn authenticated_user_comes_from_validated_basic_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode("dependaboard:secret"))
                .parse()
                .unwrap(),
        );
        assert_eq!(
            authenticated_dashboard_user(&headers, "dependaboard", "secret"),
            Some(UserId::new("dependaboard"))
        );
        assert_eq!(
            authenticated_dashboard_user(&headers, "dependaboard", "wrong"),
            None
        );
    }

    #[test]
    fn typed_webhooks_are_reduced_to_routing_fields() {
        let installation = parsed_webhook(
            "installation",
            serde_json::json!({
                "action": "created",
                "installation": installation(),
                "repositories": []
            }),
        );
        assert_eq!(installation.action.as_deref(), Some("created"));
        assert_eq!(installation.installation_id, Some(42));

        let repositories = parsed_webhook(
            "installation_repositories",
            with_common(serde_json::json!({
                "action": "removed",
                "repositories_added": [],
                "repositories_removed": [],
                "repository_selection": "all"
            })),
        );
        assert_eq!(repositories.action.as_deref(), Some("removed"));

        let pull_request = parsed_webhook(
            "pull_request",
            with_common(serde_json::json!({
                "action": "synchronize",
                "number": 9,
                "pull_request": {
                    "id": 900,
                    "number": 9,
                    "url": "https://api.github.test/repos/acme/api/pulls/9",
                    "head": { "ref": "dependabot/update", "sha": "abc123" },
                    "base": { "ref": "main", "sha": "base123" }
                }
            })),
        );
        assert_eq!(pull_request.number, Some(9));
        assert_eq!(pull_request.sha.as_deref(), Some("abc123"));

        for (event_name, object_name) in
            [("check_run", "check_run"), ("check_suite", "check_suite")]
        {
            let mut payload = with_common(serde_json::json!({ "action": "completed" }));
            payload[object_name] = serde_json::json!({
                "head_sha": "abc123",
                "pull_requests": [{ "number": 9 }, { "number": 10 }]
            });
            let event = parsed_webhook(event_name, payload);
            assert_eq!(event.action.as_deref(), Some("completed"));
            assert_eq!(event.sha.as_deref(), Some("abc123"));
            assert_eq!(event.pull_requests, vec![9, 10]);
        }

        let status = parsed_webhook(
            "status",
            with_common(serde_json::json!({
                "avatar_url": null,
                "branches": [],
                "commit": {},
                "context": "ci/test",
                "created_at": "2026-01-01T00:00:00Z",
                "description": null,
                "id": 1,
                "name": "ci/test",
                "sha": "abc123",
                "state": "success",
                "target_url": null,
                "updated_at": "2026-01-01T00:00:00Z"
            })),
        );
        assert_eq!(status.sha.as_deref(), Some("abc123"));
        assert_eq!(status.action, None);

        for event in [repositories, pull_request, status] {
            assert_eq!(event.installation_id, Some(42));
            assert_eq!(event.repository_id, Some(7));
            assert_eq!(event.owner.as_deref(), Some("acme"));
            assert_eq!(event.repo.as_deref(), Some("api"));
        }
    }

    #[test]
    fn malformed_known_webhook_is_rejected() {
        assert!(parse_webhook("pull_request", br#"{"action":"opened"}"#).is_err());
        let mut check_run = with_common(serde_json::json!({
            "action": "completed",
            "check_run": {
                "head_sha": "abc123",
                "pull_requests": [{}]
            }
        }));
        assert!(parse_webhook("check_run", &serde_json::to_vec(&check_run).unwrap()).is_err());
        check_run["check_run"]["pull_requests"] = serde_json::json!([]);
        check_run["check_run"]
            .as_object_mut()
            .unwrap()
            .remove("head_sha");
        assert!(parse_webhook("check_run", &serde_json::to_vec(&check_run).unwrap()).is_err());
    }

    #[test]
    fn pull_request_status_path_encodes_the_object_key_separator() {
        assert_eq!(pr_status_path(7, 9), "PullRequest/7%239/status");
    }

    #[test]
    fn manual_pull_request_sync_uses_the_dashboard_ingress_action() {
        let event = manual_pr_sync_event(
            42,
            7,
            "acme".to_owned(),
            "api".to_owned(),
            9,
            "abc123".to_owned(),
            "sync-123".to_owned(),
        );

        assert_eq!(event.event, "pull_request");
        assert_eq!(event.action.as_deref(), Some(DASHBOARD_SYNC_ACTION));
        assert_eq!(event.installation_id, Some(42));
        assert_eq!(event.repository_id, Some(7));
        assert_eq!(event.owner.as_deref(), Some("acme"));
        assert_eq!(event.repo.as_deref(), Some("api"));
        assert_eq!(event.number, Some(9));
        assert_eq!(event.sha.as_deref(), Some("abc123"));
        assert_eq!(event.sync_completion_id.as_deref(), Some("sync-123"));
    }

    #[test]
    fn pull_request_sync_completes_only_for_its_request_id() {
        let mut state = PrState::default();
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-456".to_owned());
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-123".to_owned());
        assert!(sync_id_completed(Some(&state), "sync-123"));
    }

    fn parsed_webhook(event_name: &str, payload: Value) -> WebhookEvent {
        parse_webhook(event_name, &serde_json::to_vec(&payload).unwrap()).unwrap()
    }

    fn with_common(mut payload: Value) -> Value {
        payload["installation"] = installation();
        payload["repository"] = repository();
        payload
    }

    fn installation() -> Value {
        serde_json::json!({ "id": 42, "node_id": "I_42" })
    }

    fn repository() -> Value {
        serde_json::json!({
            "id": 7,
            "name": "api",
            "url": "https://api.github.test/repos/acme/api",
            "owner": author("acme")
        })
    }

    fn author(login: &str) -> Value {
        serde_json::json!({
            "login": login,
            "id": 1,
            "node_id": "U_1",
            "avatar_url": "https://github.test/avatar",
            "gravatar_id": "",
            "url": "https://api.github.test/users/acme",
            "html_url": "https://github.test/acme",
            "followers_url": "https://api.github.test/users/acme/followers",
            "following_url": "https://api.github.test/users/acme/following{/other_user}",
            "gists_url": "https://api.github.test/users/acme/gists{/gist_id}",
            "starred_url": "https://api.github.test/users/acme/starred{/owner}{/repo}",
            "subscriptions_url": "https://api.github.test/users/acme/subscriptions",
            "organizations_url": "https://api.github.test/users/acme/orgs",
            "repos_url": "https://api.github.test/users/acme/repos",
            "events_url": "https://api.github.test/users/acme/events{/privacy}",
            "received_events_url": "https://api.github.test/users/acme/received_events",
            "type": "User",
            "site_admin": false,
            "name": null,
            "patch_url": null
        })
    }
}
