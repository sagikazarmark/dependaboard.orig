#![allow(non_snake_case)]

use std::collections::BTreeSet;

use dependaboard_core::{
    BatchProgress, BulkActionKind, BulkRequest, CheckStatus, DashboardPage, MergeMethod, Page,
    PrFilter, PrRecord, PrTarget, TargetProgressState, UpdateType, new_batch_id,
};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use {
    axum::{
        body::{Body, Bytes},
        http::{HeaderMap, Request, StatusCode, header},
        middleware::{self, Next},
        response::{IntoResponse, Response},
        routing::post,
    },
    base64::{Engine as _, engine::general_purpose::STANDARD},
    dependaboard_core::WebhookEvent,
    dependaboard_store::{LibSqlPrStore, PrStore, StoreConfig},
    dioxus::server::{DioxusRouterExt, ServeConfig},
    hmac::{Hmac, Mac},
    serde::{Serialize, de::DeserializeOwned},
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

#[server]
async fn submit_batch(batch_id: String, mut request: BulkRequest) -> Result<(), ServerFnError> {
    if !dependaboard_core::valid_batch_id(&batch_id) {
        return Err(ServerFnError::new("batch id must be a UUIDv7"));
    }
    if request.targets.is_empty() || request.targets.len() > dependaboard_core::MAX_BATCH_TARGETS {
        return Err(ServerFnError::new(format!(
            "batch must contain between 1 and {} targets",
            dependaboard_core::MAX_BATCH_TARGETS
        )));
    }
    let unique = request
        .targets
        .iter()
        .map(PrTarget::key)
        .collect::<BTreeSet<_>>();
    if unique.len() != request.targets.len() {
        return Err(ServerFnError::new("batch contains duplicate pull requests"));
    }
    request.user_id = "dashboard".to_owned();
    restate_send(&format!("BulkAction/{batch_id}/run"), &request)
        .await
        .map_err(ServerFnError::new)
}

#[server]
async fn load_batch_progress(batch_id: String) -> Result<Option<BatchProgress>, ServerFnError> {
    if !dependaboard_core::valid_batch_id(&batch_id) {
        return Err(ServerFnError::new("batch id must be a UUIDv7"));
    }
    restate_call(&format!("BulkAction/{batch_id}/progress"), &())
        .await
        .map_err(ServerFnError::new)
}

#[server]
async fn request_sync() -> Result<(), ServerFnError> {
    let installation_id = std::env::var("GITHUB_INSTALLATION_ID")
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID is not configured"))?
        .parse::<u64>()
        .map_err(|_| ServerFnError::new("GITHUB_INSTALLATION_ID must be an integer"))?;
    let event = WebhookEvent {
        event: "installation_repositories".to_owned(),
        action: Some("dashboard_sync".to_owned()),
        installation_id: Some(installation_id),
        repository_id: None,
        owner: None,
        repo: None,
        number: None,
        sha: None,
        pull_requests: Vec::new(),
    };
    restate_send("WebhookIngress/dispatch", &event)
        .await
        .map_err(ServerFnError::new)
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
        .or_else(|_| std::env::var("RESTATE_API_KEY"))
        .ok()
        .filter(|value| !value.is_empty());
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
async fn restate_call<T, R>(path: &str, input: &T) -> Result<R, String>
where
    T: Serialize + ?Sized,
    R: DeserializeOwned,
{
    let (client, base, token) = restate_client()?;
    let mut request = client
        .post(format!("{base}/restate/call/{path}"))
        .json(input);
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
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON payload").into_response(),
    };
    let event = normalize_webhook(event_name, &payload);
    match restate_send("WebhookIngress/dispatch", &event).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            tracing::error!(%error, event = event_name, "Restate rejected webhook");
            (StatusCode::BAD_GATEWAY, "could not enqueue webhook").into_response()
        }
    }
}

#[cfg(feature = "server")]
async fn require_dashboard_auth(request: Request<Body>, next: Next) -> Response {
    let username =
        std::env::var("DASHBOARD_USERNAME").unwrap_or_else(|_| "dependaboard".to_owned());
    let password = std::env::var("DASHBOARD_PASSWORD").unwrap_or_default();
    let authenticated = request
        .headers()
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
        .is_some_and(|(user, pass)| {
            constant_time_eq(user.as_bytes(), username.as_bytes())
                && constant_time_eq(pass.as_bytes(), password.as_bytes())
        });
    if authenticated {
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
fn normalize_webhook(event: &str, payload: &Value) -> WebhookEvent {
    let nested = |path: &[&str]| {
        path.iter()
            .fold(Some(payload), |value, key| value?.get(*key))
    };
    let sha = match event {
        "pull_request" => nested(&["pull_request", "head", "sha"]),
        "check_run" => nested(&["check_run", "head_sha"]),
        "check_suite" => nested(&["check_suite", "head_sha"]),
        "status" => payload.get("sha"),
        _ => None,
    }
    .and_then(Value::as_str)
    .map(ToOwned::to_owned);
    let pull_requests = match event {
        "check_run" => nested(&["check_run", "pull_requests"]),
        "check_suite" => nested(&["check_suite", "pull_requests"]),
        _ => None,
    }
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
    .filter_map(|pull| pull.get("number").and_then(Value::as_u64))
    .collect();

    WebhookEvent {
        event: event.to_owned(),
        action: payload
            .get("action")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        installation_id: nested(&["installation", "id"]).and_then(Value::as_u64),
        repository_id: nested(&["repository", "id"]).and_then(Value::as_u64),
        owner: nested(&["repository", "owner", "login"])
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        repo: nested(&["repository", "name"])
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        number: nested(&["pull_request", "number"])
            .or_else(|| payload.get("number"))
            .and_then(Value::as_u64),
        sha,
        pull_requests,
    }
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
    let mut merge_method = use_signal(MergeMethod::default);
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
                DetailDrawer { row, onclose: move |_| detail.set(None) }
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
                    merge_method: merge_method(),
                    onmethod: move |method| merge_method.set(method),
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
                            let request = BulkRequest {
                                action,
                                targets,
                                merge_method: merge_method(),
                                user_id: "dashboard".to_owned(),
                            };
                            active_batch.set(Some(BatchProgress::queued(&batch_id, &request)));
                            progress_open.set(true);
                            confirm.set(None);
                            selected.write().clear();
                            spawn(async move {
                                loop {
                                    match submit_batch(batch_id.clone(), request.clone()).await {
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
fn DetailDrawer(row: PrRecord, onclose: EventHandler<MouseEvent>) -> Element {
    let stale = unix_seconds().saturating_sub(row.synced_at) > 45 * 60;
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
                }
                a { class: "drawer-link", href: row.html_url, target: "_blank", rel: "noreferrer", "Open on GitHub" }
            }
        }
    }
}

#[component]
fn ConfirmModal(
    action: BulkActionKind,
    count: usize,
    merge_method: MergeMethod,
    onmethod: EventHandler<MergeMethod>,
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
                    label { class: "method-field",
                        span { "Merge method" }
                        select {
                            class: "select select-sm",
                            value: merge_method.to_string(),
                            onchange: move |event| {
                                let method = match event.value().as_str() {
                                    "merge" => MergeMethod::Merge,
                                    "rebase" => MergeMethod::Rebase,
                                    _ => MergeMethod::Squash,
                                };
                                onmethod.call(method);
                            },
                            option { value: "squash", "Squash" }
                            option { value: "merge", "Merge commit" }
                            option { value: "rebase", "Rebase and merge" }
                        }
                    }
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
                            div { strong { "{item.target.owner}/{item.target.repo} #{item.target.number}" } small { "{progress_detail(&item.state)}" } }
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
    }
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn wait_one_second() {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(1_000).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

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
    fn check_run_payload_is_reduced_to_routing_fields() {
        let payload = serde_json::json!({
            "action": "completed",
            "installation": { "id": 42 },
            "repository": {
                "id": 7,
                "name": "api",
                "owner": { "login": "acme" }
            },
            "check_run": {
                "head_sha": "abc123",
                "pull_requests": [{ "number": 9 }, { "number": 10 }]
            }
        });

        assert_eq!(
            normalize_webhook("check_run", &payload),
            WebhookEvent {
                event: "check_run".to_owned(),
                action: Some("completed".to_owned()),
                installation_id: Some(42),
                repository_id: Some(7),
                owner: Some("acme".to_owned()),
                repo: Some("api".to_owned()),
                number: None,
                sha: Some("abc123".to_owned()),
                pull_requests: vec![9, 10],
            }
        );
    }
}
