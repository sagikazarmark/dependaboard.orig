#![allow(non_snake_case)]

use std::collections::BTreeSet;

use dependaboard_core::{
    BatchProgress, BulkActionKind, CheckStatus, DashboardPage, LabelFacet, Page, PrFilter,
    PrRecord, PrState, PrTarget, TargetProgressState, UpdateType, new_batch_id, unix_seconds,
};
use dioxus::prelude::*;

// Installed from the dioxus-daisyui-components registry with `dx components add`;
// each component carries its full daisyUI axis, so unused variants are expected.
#[allow(dead_code, unused_imports)]
mod components;

use components::alert_dialog::{
    AlertDialog, AlertDialogAction, AlertDialogActions, AlertDialogCancel, AlertDialogDescription,
    AlertDialogDescriptionAppearance, AlertDialogTitle, AlertDialogTitleAppearance,
};
use components::button::{Button, ButtonSize};
use components::loading::{Loading, LoadingSize};
use components::toast::{
    ToastCloseButton, ToastColor, ToastContent, ToastDescription, ToastOptions, ToastProps,
    ToastPropsWithOwner, ToastProvider, ToastTitle, ToastTitleAppearance, use_toast,
};

#[cfg(feature = "server")]
use {
    axum::{
        body::{Body, Bytes},
        extract::{Extension, State},
        http::{HeaderMap, Request, StatusCode, header},
        middleware::{self, Next},
        response::{IntoResponse, Response},
        routing::post,
    },
    base64::{Engine as _, engine::general_purpose::STANDARD},
    dependaboard_core::{BulkRequest, DASHBOARD_SYNC_ACTION, PrKey, UserId, WebhookEvent},
    dependaboard_store::{LibSqlPrStore, PrStore, StoreConfig},
    dioxus::server::{DioxusRouterExt, ServeConfig},
    octoevents::{Envelope, EventKind, ResponseStatus, Secret, Verifier},
    serde::{Deserialize, Serialize, de::DeserializeOwned},
    serde_json::Value,
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
        .with_state(webhook_verifier())
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

/// Builds the webhook verifier once, so a missing secret fails at startup
/// rather than at the first delivery.
#[cfg(feature = "server")]
fn webhook_verifier() -> Verifier {
    let secret =
        std::env::var("GITHUB_WEBHOOK_SECRET").expect("GITHUB_WEBHOOK_SECRET must be configured");
    assert!(
        !secret.trim().is_empty(),
        "GITHUB_WEBHOOK_SECRET must not be empty"
    );
    Verifier::new(Secret::new(secret))
}

#[cfg(feature = "server")]
async fn github_webhook(
    State(verifier): State<Verifier>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let envelope = match Envelope::from_signed_parts(&verifier, &headers, body) {
        Ok(envelope) => envelope,
        Err(error) => {
            tracing::warn!(%error, "GitHub webhook delivery was rejected");
            let status = StatusCode::from(ResponseStatus::for_receive_error(&error));
            return (status, "webhook delivery was rejected").into_response();
        }
    };
    // GitHub pings a new App before any real delivery; nothing downstream routes it.
    if envelope.kind == EventKind::Ping {
        return StatusCode::NO_CONTENT.into_response();
    }
    let event = match routed_event(&envelope) {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(%error, event = %envelope.kind, "GitHub webhook payload was rejected");
            return (StatusCode::BAD_REQUEST, "invalid GitHub webhook payload").into_response();
        }
    };
    match restate_send("WebhookIngress/dispatch", &event).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::error!(%error, event = %envelope.kind, "Restate rejected webhook");
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

/// Reduces a verified envelope to the routing fields `WebhookIngress` dispatches on.
///
/// The envelope already carries the installation and repository probe, so only
/// the per-event fields — PR number, head SHA, and the PRs a check belongs to —
/// need parsing out of the payload.
#[cfg(feature = "server")]
fn routed_event(envelope: &Envelope) -> Result<WebhookEvent, String> {
    let (number, sha, pull_requests) = match envelope.kind {
        EventKind::Installation | EventKind::InstallationRepositories => {
            if envelope.common.installation_id.is_none() {
                return Err("GitHub installation webhook is missing an installation id".to_owned());
            }
            (None, None, Vec::new())
        }
        EventKind::PullRequest => {
            let payload: PullRequestRouting = parse_payload(envelope)?;
            (
                Some(payload.pull_request.number),
                Some(payload.pull_request.head.sha),
                Vec::new(),
            )
        }
        EventKind::CheckRun => {
            let payload: CheckRunRouting = parse_payload(envelope)?;
            let (sha, numbers) = payload.check_run.into_routing();
            (None, Some(sha), numbers)
        }
        EventKind::CheckSuite => {
            let payload: CheckSuiteRouting = parse_payload(envelope)?;
            let (sha, numbers) = payload.check_suite.into_routing();
            (None, Some(sha), numbers)
        }
        EventKind::Status => {
            let payload: StatusRouting = parse_payload(envelope)?;
            (None, Some(payload.sha), Vec::new())
        }
        _ => (None, None, Vec::new()),
    };
    let repository = match envelope.kind {
        EventKind::PullRequest
        | EventKind::CheckRun
        | EventKind::CheckSuite
        | EventKind::Status => Some(
            envelope
                .common
                .repository
                .as_ref()
                .ok_or("GitHub repository webhook is missing routing fields")?,
        ),
        _ => envelope.common.repository.as_ref(),
    };
    Ok(WebhookEvent {
        event: envelope.kind.as_str().to_owned(),
        action: envelope
            .action
            .as_ref()
            .map(|action| action.as_str().to_owned()),
        installation_id: envelope.common.installation_id,
        repository_id: repository.map(|repository| repository.id),
        owner: repository.map(|repository| repository.owner.clone()),
        repo: repository.map(|repository| repository.name.clone()),
        number,
        sha,
        pull_requests,
        sync_completion_id: None,
    })
}

#[cfg(feature = "server")]
fn parse_payload<T: DeserializeOwned>(envelope: &Envelope) -> Result<T, String> {
    envelope.parse::<T>().map_err(|error| error.to_string())
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct PullRequestRouting {
    pull_request: PullRequestRef,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct PullRequestRef {
    number: u64,
    head: CommitRef,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct CommitRef {
    sha: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct CheckRunRouting {
    check_run: CheckRouting,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct CheckSuiteRouting {
    check_suite: CheckRouting,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct StatusRouting {
    sha: String,
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct CheckRouting {
    head_sha: String,
    #[serde(default)]
    pull_requests: Vec<CheckPullRequest>,
}

#[cfg(feature = "server")]
impl CheckRouting {
    fn into_routing(self) -> (String, Vec<u64>) {
        let numbers = self
            .pull_requests
            .into_iter()
            .map(|pull| pull.number)
            .collect();
        (self.head_sha, numbers)
    }
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
struct CheckPullRequest {
    number: u64,
}

/// A bulk action the user has asked for but not yet confirmed.
///
/// The targets are resolved when the request is made, so the head SHAs the
/// confirmation dialog talks about are the ones that get submitted, and the
/// dialog's cancel and confirm paths do not depend on each other's ordering.
#[derive(Clone, PartialEq)]
struct PendingAction {
    action: BulkActionKind,
    targets: Vec<PrTarget>,
}

fn App() -> Element {
    let dark = use_signal(|| true);
    let theme = if dark() {
        "dependaboard-dark"
    } else {
        "dependaboard-light"
    };

    rsx! {
        document::Link { rel: "preconnect", href: "https://fonts.googleapis.com" }
        document::Link { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "anonymous" }
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;600&family=IBM+Plex+Sans:wght@400;500;600&display=swap"
        }
        document::Stylesheet { href: asset!("/assets/main.css") }

        // The toast region renders where the provider sits, so it has to be
        // inside the themed shell for daisyUI's colour variables to reach it.
        div { class: "app-shell", "data-theme": theme,
            ToastProvider {
                render_toast: Callback::new(|props: ToastPropsWithOwner| rsx! { AppToast { ..props } }),
                Dashboard { dark }
            }
        }
    }
}

/// The registry toast with this app's chrome: the message is the title, set in
/// the same weight as the rest of the UI, and `toast-message` carries the look.
fn AppToast(props: ToastProps) -> Element {
    let color = ToastColor::of(props.toast_type).class();
    rsx! {
        dioxus_primitives::toast::Toast {
            id: props.id,
            index: props.index,
            title: props.title,
            description: props.description,
            toast_type: props.toast_type,
            on_close: props.on_close,
            permanent: props.permanent,
            duration: props.duration,
            class: "alert {color} toast-message",
            ToastContent {
                ToastTitle { appearance: ToastTitleAppearance::None }
                ToastDescription {}
            }
            ToastCloseButton {}
        }
    }
}

#[component]
fn Dashboard(mut dark: Signal<bool>) -> Element {
    let toast = use_toast();
    let mut aside_open = use_signal(|| true);
    let mut filter = use_signal(PrFilter::default);
    let mut cursor = use_signal(|| None::<String>);
    let mut refresh = use_signal(|| 0_u64);
    let mut selected = use_signal(BTreeSet::<String>::new);
    let mut detail = use_signal(|| None::<PrRecord>);
    let mut pending = use_signal(|| None::<PendingAction>);
    let mut active_batch = use_signal(|| None::<BatchProgress>);
    let mut progress_open = use_signal(|| false);

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
    let active_filter_count = filter_count(&filter());

    let mut reload = move || {
        refresh += 1;
        dashboard.restart();
    };

    let queue_batch = move |PendingAction { action, targets }: PendingAction| {
        let batch_id = new_batch_id();
        active_batch.set(Some(BatchProgress::queued(&batch_id, action, &targets)));
        progress_open.set(true);
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
                        toast.warning(
                            format!("Batch submission interrupted; retrying: {error}"),
                            ToastOptions::new(),
                        );
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
                            match failed {
                                Some(failure) => {
                                    toast.error(format!("Batch failed: {failure}"), sticky())
                                }
                                None => {
                                    toast.success("Batch complete".to_owned(), ToastOptions::new())
                                }
                            }
                            reload();
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        toast.warning(
                            format!("Progress interrupted; retrying: {error}"),
                            ToastOptions::new(),
                        );
                    }
                }
            }
        });
    };

    rsx! {
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
            Button {
                size: ButtonSize::Xs,
                class: "btn-ghost theme-button",
                title: "Toggle color theme",
                onclick: move |_| dark.toggle(),
                if dark() { "light" } else { "dark" }
            }
            Button {
                size: ButtonSize::Sm,
                class: "sync-button",
                onclick: move |_| {
                    spawn(async move {
                        if let Err(error) = request_sync().await {
                            toast.error(format!("Sync failed: {error}"), sticky());
                        } else {
                            toast.info("Reconciliation queued".to_owned(), ToastOptions::new());
                            wait_one_second().await;
                            reload();
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
                            count: page.as_ref().map_or(0, |page| page.facets.check_count(status)),
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
                            count: page.as_ref().map_or(0, |page| page.facets.update_type_count(update_type)),
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
                LabelFacets {
                    labels: page.as_ref().map(|page| page.facets.labels.clone()).unwrap_or_default(),
                    active: filter().labels,
                    ontoggle: move |label| {
                        toggle_value(&mut filter.write().labels, label);
                        cursor.set(None);
                    },
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
                            Button { size: ButtonSize::Sm, onclick: move |_| dashboard.restart(), "Retry" }
                        }
                    } else if page.is_none() {
                        div { class: "loading-state",
                            Loading { size: LoadingSize::Sm }
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
                            Button {
                                size: ButtonSize::Sm,
                                class: "btn-ghost",
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
                Button {
                    size: ButtonSize::Sm,
                    class: "rebase-button",
                    onclick: {
                        let rows = rows.clone();
                        move |_| pending.set(Some(PendingAction {
                            action: BulkActionKind::Rebase,
                            targets: selected_targets(&rows, &selected.read()),
                        }))
                    },
                    "Request rebase"
                }
                Button {
                    size: ButtonSize::Sm,
                    class: "merge-button",
                    onclick: {
                        let rows = rows.clone();
                        move |_| pending.set(Some(PendingAction {
                            action: BulkActionKind::Merge,
                            targets: selected_targets(&rows, &selected.read()),
                        }))
                    },
                    "Merge selected"
                }
            }
        }

        if let Some(row) = detail() {
            DetailDrawer {
                row,
                onclose: move |_| detail.set(None),
                onaction: move |action| pending.set(Some(action)),
                onsync: move |result: Result<Option<PrRecord>, String>| match result {
                    Ok(Some(row)) => {
                        detail.set(Some(row));
                        toast.success("Pull request synced".to_owned(), ToastOptions::new());
                        reload();
                    }
                    Ok(None) => {
                        detail.set(None);
                        toast.info("Pull request is no longer open".to_owned(), ToastOptions::new());
                        reload();
                    }
                    Err(error) => toast.error(error, sticky()),
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

        ConfirmModal {
            pending: pending(),
            oncancel: move |_| pending.set(None),
            onconfirm: queue_batch,
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

/// How many of the ranked labels the sidebar shows before cutting off.
const LABEL_FACET_LIMIT: usize = 8;

/// The most common labels as chips. `labels` arrives already ranked by the
/// store (descending count, then name) and is rendered in that order.
#[component]
fn LabelFacets(
    labels: Vec<LabelFacet>,
    active: Vec<String>,
    ontoggle: EventHandler<String>,
) -> Element {
    rsx! {
        div { class: "label-facets",
            for facet in labels.into_iter().take(LABEL_FACET_LIMIT) {
                {
                    let is_active = active.contains(&facet.label);
                    let label_value = facet.label.clone();
                    rsx! {
                        button {
                            key: "label-{facet.label}",
                            class: if is_active { "label-filter active" } else { "label-filter" },
                            onclick: move |_| ontoggle.call(label_value.clone()),
                            "{facet.label} " span { "{facet.count}" }
                        }
                    }
                }
            }
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
    let versions = version_label(row.from_version.as_deref(), row.to_version.as_deref());
    let stale = row.is_stale(unix_seconds());
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

/// The scrim, panel and header shared by the side drawers. Clicking the scrim
/// or the close button closes; clicks inside the panel do not propagate.
#[component]
fn SidePanel(
    class: &'static str,
    eyebrow: String,
    title: Element,
    onclose: EventHandler<()>,
    children: Element,
) -> Element {
    rsx! {
        div { class: "drawer-scrim", onclick: move |_| onclose.call(()),
            section { class: "side-drawer {class}", onclick: move |event| event.stop_propagation(),
                div { class: "drawer-head",
                    div {
                        span { class: "eyebrow", "{eyebrow}" }
                        h2 { {title} }
                    }
                    button { class: "close-button", onclick: move |_| onclose.call(()), "x" }
                }
                {children}
            }
        }
    }
}

#[component]
fn DetailDrawer(
    row: PrRecord,
    onclose: EventHandler<()>,
    onaction: EventHandler<PendingAction>,
    onsync: EventHandler<Result<Option<PrRecord>, String>>,
) -> Element {
    let stale = row.is_stale(unix_seconds());
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
    let target = pr_target(&row);
    let request = use_callback(move |action: BulkActionKind| {
        onaction.call(PendingAction {
            action,
            targets: vec![target.clone()],
        });
    });
    rsx! {
        SidePanel {
            class: "detail-drawer",
            eyebrow: "Pull request",
            onclose,
            title: rsx! {
                a {
                    class: "github-pr-link",
                    href: row.html_url.clone(),
                    target: "_blank",
                    rel: "noreferrer",
                    "{row.owner}/{row.repo}#{row.number}"
                    span { class: "external-link-glyph", "↗" }
                }
            },
            div { class: "drawer-body",
                h3 { "{row.title}" }
                div { class: "drawer-badges",
                    span { class: "update-chip {update_class(row.update_type)}", "{row.update_type}" }
                    span { class: "status-badge", span { class: "check-dot {status_class(row.check_status)}" } "{status_label(row.check_status)}" }
                    span { class: "status-badge", "{row.mergeable}" }
                    if stale { span { class: "status-badge stale-badge", "projection stale" } }
                }
                div { class: "drawer-actions",
                    Button {
                        size: ButtonSize::Sm,
                        class: "rebase-button",
                        onclick: move |_| request(BulkActionKind::Rebase),
                        "Rebase"
                    }
                    Button {
                        size: ButtonSize::Sm,
                        class: "merge-button",
                        onclick: move |_| request(BulkActionKind::Merge),
                        "Merge"
                    }
                    Button {
                        size: ButtonSize::Sm,
                        class: "drawer-sync",
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
                            "Queueing..."
                        } else if sync_queued() {
                            "Syncing..."
                        } else {
                            "Sync"
                        }
                    }
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
                        Loading { size: LoadingSize::Sm }
                        "Reading durable activity"
                    }
                }
            }
        }
    }
}

/// The confirmation for a bulk action. Always mounted with a controlled open
/// state so the dialog restores focus when it closes; both buttons close the
/// dialog before their handler runs, so `oncancel` also fires ahead of
/// `onconfirm` and must not hold anything the latter needs.
#[component]
fn ConfirmModal(
    pending: Option<PendingAction>,
    oncancel: EventHandler<()>,
    onconfirm: EventHandler<PendingAction>,
) -> Element {
    rsx! {
        AlertDialog {
            id: "confirm-bulk-action",
            class: "confirm-box",
            open: Some(pending.is_some()),
            on_open_change: move |open: bool| {
                if !open {
                    oncancel.call(());
                }
            },
            if let Some(pending) = pending {
                {
                    let action = pending.action;
                    let count = pending.targets.len();
                    rsx! {
                        span { class: "eyebrow", "Durable bulk action" }
                        AlertDialogTitle { appearance: AlertDialogTitleAppearance::None,
                            "{action} {count} pull requests?"
                        }
                        AlertDialogDescription { appearance: AlertDialogDescriptionAppearance::None,
                            "The selected head SHAs are captured now. Moved or ineligible pull requests will be rejected, not silently retried against new code."
                        }
                        if action == BulkActionKind::Merge {
                            div { class: "notice", "Uses the globally configured merge method." }
                        } else {
                            div { class: "notice", "Rebase is requested by an idempotent @dependabot comment using the configured user token." }
                        }
                        AlertDialogActions {
                            AlertDialogCancel { class: "btn-ghost btn-sm", "Cancel" }
                            AlertDialogAction {
                                class: "btn-sm confirm-button",
                                on_click: move |_| onconfirm.call(pending.clone()),
                                "Queue {action}"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ProgressDrawer(progress: BatchProgress, onclose: EventHandler<()>) -> Element {
    let completed = progress.succeeded + progress.rejected;
    let total = progress.targets.len();
    let percentage = if total == 0 {
        100
    } else {
        completed * 100 / total as u64
    };
    rsx! {
        SidePanel {
            class: "progress-drawer",
            eyebrow: "Batch {progress.batch_id}",
            title: rsx! { "{progress.action} progress" },
            onclose,
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
                                    class: "github-pr-link",
                                    href: html_url,
                                    target: "_blank",
                                    rel: "noreferrer",
                                    strong { "{item.target.owner}/{item.target.repo}#{item.target.number}" }
                                    span { class: "external-link-glyph", "↗" }
                                }
                            } else {
                                strong { "{item.target.owner}/{item.target.repo}#{item.target.number}" }
                            }
                            small { "{progress_detail(&item.state)}" }
                        }
                    }
                }
            }
        }
    }
}

/// Errors stay until dismissed; every other toast auto-dismisses.
fn sticky() -> ToastOptions {
    ToastOptions::new().permanent(true)
}

fn selected_targets(rows: &[PrRecord], selected: &BTreeSet<String>) -> Vec<PrTarget> {
    rows.iter()
        .filter(|row| selected.contains(&row.id))
        .map(pr_target)
        .collect()
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
    use dependaboard_core::Mergeable;

    use super::*;

    const GROUPED_ROW_TITLE: &str =
        "build(deps): bump the github-actions group across 1 directory with 3 updates";

    fn grouped_row() -> PrRecord {
        PrRecord {
            id: "7#9".to_owned(),
            repository_id: 7,
            installation_id: 1,
            owner: "acme".to_owned(),
            repo: "api".to_owned(),
            number: 9,
            title: GROUPED_ROW_TITLE.to_owned(),
            html_url: "https://github.example/acme/api/pull/9".to_owned(),
            dependency: None,
            from_version: None,
            to_version: None,
            dependencies: ["actions/checkout", "actions/cache", "actions/setup-rust"]
                .into_iter()
                .map(|name| dependaboard_core::DependencyUpdate {
                    name: name.to_owned(),
                    from_version: None,
                    to_version: None,
                    update_type: UpdateType::Minor,
                })
                .collect(),
            update_type: UpdateType::Minor,
            head_sha: "abc123".to_owned(),
            check_status: CheckStatus::Failure,
            mergeable: Mergeable::Clean,
            labels: vec!["dependencies".to_owned(), "github_actions".to_owned()],
            created_at: 1,
            updated_at: 1,
            synced_at: unix_seconds(),
        }
    }

    fn GroupedRowFixture() -> Element {
        rsx! {
            PrRow {
                row: grouped_row(),
                checked: false,
                oncheck: move |_| {},
                onopen: move |_| {},
            }
        }
    }

    fn DrawerFixture() -> Element {
        let mut row = grouped_row();
        // `has_hooks` is the one state whose display form differs from its
        // variant name, so the assertion below can tell Display from Debug.
        row.mergeable = Mergeable::HasHooks;
        rsx! {
            DetailDrawer {
                row,
                onclose: move |_| {},
                onaction: move |_| {},
                onsync: move |_| {},
            }
        }
    }

    fn LabelFacetsFixture() -> Element {
        // Nine labels already ranked by the store, deliberately not in
        // alphabetical order, so the component can only pass by keeping the
        // sequence it was given and cutting it at eight.
        let labels = [
            ("rust", 9),
            ("go", 7),
            ("security", 7),
            ("dependencies", 5),
            ("python", 4),
            ("java", 3),
            ("javascript", 2),
            ("blocked", 1),
            ("actions", 1),
        ]
        .into_iter()
        .map(|(label, count)| LabelFacet {
            label: label.to_owned(),
            count,
        })
        .collect();
        rsx! {
            LabelFacets {
                labels,
                active: vec!["go".to_owned()],
                ontoggle: move |_| {},
            }
        }
    }

    #[test]
    fn grouped_row_does_not_repeat_a_long_title_as_its_version() {
        let mut dom = VirtualDom::new(GroupedRowFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        assert_eq!(html.matches(GROUPED_ROW_TITLE).count(), 1, "{html}");
        assert!(html.contains("<strong>3 dependencies</strong><span>group update</span>"));
    }

    #[test]
    fn detail_drawer_shows_the_mergeable_state_in_its_display_form() {
        let mut dom = VirtualDom::new(DrawerFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        assert!(
            html.contains(r#"<span class="status-badge">has_hooks</span>"#),
            "{html}"
        );
    }

    #[test]
    fn label_facets_keep_the_store_ranking_and_show_the_top_eight() {
        let mut dom = VirtualDom::new(LabelFacetsFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        let positions = [
            "rust <span>9</span>",
            "go <span>7</span>",
            "security <span>7</span>",
            "dependencies <span>5</span>",
            "python <span>4</span>",
            "java <span>3</span>",
            "javascript <span>2</span>",
            "blocked <span>1</span>",
        ]
        .map(|chip| {
            html.find(chip)
                .unwrap_or_else(|| panic!("{chip} missing in {html}"))
        });
        assert!(positions.is_sorted(), "{html}");
        assert!(!html.contains("actions"), "{html}");
        assert!(
            html.contains(r#"<button class="label-filter active">go "#),
            "{html}"
        );
    }

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
    fn webhook_deliveries_are_authenticated_before_they_are_routed() {
        // openssl dgst -sha256 -hmac secret, over exactly the bytes below.
        const BODY: &[u8] = br#"{"action":"opened","installation":{"id":42}}"#;
        const SIGNATURE: &str =
            "sha256=015a17fc63d8f4eb2ffd3f3f70444a66af82856c318e854607c77d1747a3d3c9";

        let body = Bytes::from_static(BODY);
        let headers = |signature: &'static str| {
            octoevents::HeaderView::new()
                .signature(signature)
                .delivery_id("72d3162e-cc78-11e3-81ab-4c9367dc0958")
                .event_name("installation")
                .content_type("application/json")
        };

        let envelope = Envelope::from_signed(
            &Verifier::new(Secret::new("secret")),
            &headers(SIGNATURE),
            body.clone(),
        )
        .expect("a correctly signed delivery is accepted");
        assert_eq!(envelope.kind, EventKind::Installation);
        assert_eq!(envelope.common.installation_id, Some(42));
        assert_eq!(routed_event(&envelope).unwrap().installation_id, Some(42));

        let mismatched = Envelope::from_signed(
            &Verifier::new(Secret::new("wrong")),
            &headers(SIGNATURE),
            body.clone(),
        )
        .expect_err("a delivery signed with another secret is refused");
        assert_eq!(
            ResponseStatus::for_receive_error(&mismatched),
            ResponseStatus::Unauthorized
        );

        let malformed = Envelope::from_signed(
            &Verifier::new(Secret::new("secret")),
            &headers("sha1=abcd"),
            body,
        )
        .expect_err("a signature that is not sha256 hexadecimal is refused");
        assert_eq!(
            ResponseStatus::for_receive_error(&malformed),
            ResponseStatus::BadRequest
        );
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
    fn verified_envelopes_are_reduced_to_routing_fields() {
        let installation = routed(
            "installation",
            Some("created"),
            false,
            serde_json::json!({ "action": "created", "repositories": [] }),
        );
        assert_eq!(installation.action.as_deref(), Some("created"));
        assert_eq!(installation.installation_id, Some(42));
        assert_eq!(installation.repository_id, None);

        let repositories = routed(
            "installation_repositories",
            Some("removed"),
            true,
            serde_json::json!({
                "action": "removed",
                "repositories_added": [],
                "repositories_removed": [],
                "repository_selection": "all"
            }),
        );
        assert_eq!(repositories.action.as_deref(), Some("removed"));

        let pull_request = routed(
            "pull_request",
            Some("synchronize"),
            true,
            serde_json::json!({
                "action": "synchronize",
                "number": 9,
                "pull_request": {
                    "number": 9,
                    "head": { "ref": "dependabot/update", "sha": "abc123" },
                    "base": { "ref": "main", "sha": "base123" }
                }
            }),
        );
        assert_eq!(pull_request.number, Some(9));
        assert_eq!(pull_request.sha.as_deref(), Some("abc123"));

        for object_name in ["check_run", "check_suite"] {
            let mut payload = serde_json::json!({ "action": "completed" });
            payload[object_name] = serde_json::json!({
                "head_sha": "abc123",
                "pull_requests": [{ "number": 9 }, { "number": 10 }]
            });
            let event = routed(object_name, Some("completed"), true, payload);
            assert_eq!(event.action.as_deref(), Some("completed"));
            assert_eq!(event.sha.as_deref(), Some("abc123"));
            assert_eq!(event.pull_requests, vec![9, 10]);
        }

        let status = routed(
            "status",
            None,
            true,
            serde_json::json!({
                "context": "ci/test",
                "id": 1,
                "name": "ci/test",
                "sha": "abc123",
                "state": "success"
            }),
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
    fn envelope_missing_routing_fields_is_rejected() {
        // Authenticated but unroutable: the payload verified, yet nothing
        // downstream can address a pull request or repository with it.
        assert!(
            routed_event(&envelope(
                "pull_request",
                Some("opened"),
                true,
                &serde_json::json!({ "action": "opened" })
            ))
            .is_err()
        );
        let mut anonymous = envelope(
            "installation",
            Some("created"),
            false,
            &serde_json::json!({ "action": "created" }),
        );
        anonymous.common.installation_id = None;
        assert!(routed_event(&anonymous).is_err());

        let mut check_run = serde_json::json!({
            "action": "completed",
            "check_run": { "head_sha": "abc123", "pull_requests": [{}] }
        });
        assert!(routed_event(&envelope("check_run", Some("completed"), true, &check_run)).is_err());
        check_run["check_run"]["pull_requests"] = serde_json::json!([]);
        check_run["check_run"]
            .as_object_mut()
            .unwrap()
            .remove("head_sha");
        assert!(routed_event(&envelope("check_run", Some("completed"), true, &check_run)).is_err());

        let pull_request = serde_json::json!({
            "action": "opened",
            "number": 9,
            "pull_request": { "number": 9, "head": { "sha": "abc123" } }
        });
        assert!(
            routed_event(&envelope(
                "pull_request",
                Some("opened"),
                false,
                &pull_request
            ))
            .is_err()
        );
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

    fn routed(
        event_name: &str,
        action: Option<&str>,
        repository: bool,
        payload: Value,
    ) -> WebhookEvent {
        routed_event(&envelope(event_name, action, repository, &payload)).unwrap()
    }

    /// Builds the synthetic envelope a verified delivery would produce.
    ///
    /// `octoevents` extracts `common` from the payload itself, so the probe is
    /// mirrored here rather than re-derived: these tests cover this crate's
    /// routing, not the crate's extraction.
    fn envelope(
        event_name: &str,
        action: Option<&str>,
        repository: bool,
        payload: &Value,
    ) -> Envelope {
        let mut common = octoevents::Common::default();
        common.installation_id = Some(42);
        if repository {
            let mut reference = octoevents::RepositoryRef::default();
            reference.id = 7;
            reference.name = "api".to_owned();
            reference.full_name = "acme/api".to_owned();
            reference.owner = "acme".to_owned();
            common.repository = Some(reference);
        }
        Envelope {
            delivery_id: "72d3162e-cc78-11e3-81ab-4c9367dc0958".to_owned(),
            kind: event_name.parse().unwrap(),
            action: action.map(|action| action.parse().unwrap()),
            common,
            target_type: None,
            target_id: None,
            raw: Bytes::from(serde_json::to_vec(payload).unwrap()),
        }
    }
}
