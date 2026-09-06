//! The server functions the dashboard calls. Compiled on both targets: the
//! wasm build gets the client stubs, the server build the bodies, which reach
//! the store and Restate through [`crate::server::state::ServerState`].

use dependaboard_core::{
    BatchProgress, BatchRecord, BulkActionKind, DashboardPage, DashboardSummary, Page, PrFilter,
    PrRecord, PrState, ProjectionRevision, SubmittedTarget,
};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use {
    crate::server::{restate::pr_status_path, state::ServerState},
    axum::extract::Extension,
    dependaboard_core::{
        BulkRequest, InvalidBatch, ManualSyncRequest, PrKey, PrTarget, UserId, new_batch_id,
        validate_batch,
    },
    dependaboard_store::{PrStore, StoreError},
};

/// One page of rows for `filter`. Paging through a filter calls this alone;
/// the facets around the rows come from [`load_summary`], once per filter.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_dashboard(
    filter: PrFilter,
    page: Page,
) -> Result<DashboardPage, ServerFnError> {
    state
        .store
        .list_prs(&filter, page)
        .await
        .map_err(store_failure)
}

/// The facet counts scoped to `filter` and the read model's freshness.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_summary(filter: PrFilter) -> Result<DashboardSummary, ServerFnError> {
    state
        .store
        .dashboard_summary(&filter)
        .await
        .map_err(store_failure)
}

/// Every row `filter` matches, as far as one bulk action can take them: the
/// newest [`MAX_BATCH_TARGETS`], in the table's order, with the total so the
/// dashboard can say when the limit cut the set short. This is what "select
/// all matching" resolves to, server side, so the selection need not have
/// paged through the rows to hold them.
///
/// [`MAX_BATCH_TARGETS`]: dependaboard_core::MAX_BATCH_TARGETS
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_matching(filter: PrFilter) -> Result<DashboardPage, ServerFnError> {
    let batch = Page {
        limit: u32::try_from(dependaboard_core::MAX_BATCH_TARGETS)
            .expect("the batch limit fits in a page"),
        after: None,
    };
    state
        .store
        .list_prs(&filter, batch)
        .await
        .map_err(store_failure)
}

/// The read model's revision: a counter that moves whenever a row changes,
/// beside one that moves only when a pull request row does. Cheap enough for
/// the dashboard to poll, so it can reload the rows only when the answer has
/// moved since it last asked, and tell a sweep's first repository write from
/// its pull requests landing.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_projection_revision() -> Result<ProjectionRevision, ServerFnError> {
    state
        .store
        .projection_revision()
        .await
        .map_err(store_failure)
}

/// Asks Restate to run the batch. The batch id is the workflow key, which lets
/// a workflow run once, and the idempotency key, which lets the browser resend
/// the same submission after a lost response and be told it was accepted
/// rather than refused for the workflow already existing.
///
/// The browser names each target by key and the head it saw; the rest of the
/// target is resolved here from the projection, and a target the dashboard
/// does not know, or one from another installation, refuses the batch. The
/// submission is held to the batch rules first, so a submission that could
/// not run resolves nothing.
#[server(state: Extension<ServerState>, user: Extension<UserId>)]
pub(crate) async fn submit_batch(
    batch_id: String,
    action: BulkActionKind,
    targets: Vec<SubmittedTarget>,
) -> Result<(), ServerFnError> {
    validate_batch(&batch_id, targets.iter().map(SubmittedTarget::key))
        .map_err(|invalid| ServerFnError::new(invalid.to_string()))?;
    let mut resolved = Vec::with_capacity(targets.len());
    for target in targets {
        let row = projected_pr(&state, &target.key()).await?;
        resolved.push(PrTarget {
            repository_id: row.repository_id,
            owner: row.owner,
            repo: row.repo,
            number: row.number,
            expected_sha: target.expected_sha,
            title: row.title,
            html_url: row.html_url,
        });
    }
    let request = BulkRequest {
        action,
        targets: resolved,
        user_id: user.0,
    };
    state
        .ingress
        .send(
            &format!("BulkAction/{batch_id}/run"),
            &request,
            Some(&batch_id),
        )
        .await
        .map_err(restate_unavailable)
}

#[server(state: Extension<ServerState>)]
pub(crate) async fn load_batch_progress(
    batch_id: String,
) -> Result<Option<BatchProgress>, ServerFnError> {
    if !dependaboard_core::valid_batch_id(&batch_id) {
        return Err(ServerFnError::new(InvalidBatch::BatchId.to_string()));
    }
    state
        .ingress
        .call(&format!("BulkAction/{batch_id}/progress"))
        .await
        .map_err(restate_unavailable)
}

/// The `limit` most recently finished batches, newest first, as the projection
/// keeps them: what was asked, by whom, when, and how each target went. Read
/// from the store, not Restate, so a batch is still here after the workflow's
/// retention has cleared its progress. `limit` is held to
/// [`MAX_RECENT_BATCHES`](dependaboard_core::MAX_RECENT_BATCHES).
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_recent_batches(limit: u32) -> Result<Vec<BatchRecord>, ServerFnError> {
    state
        .store
        .recent_batches(limit.clamp(1, dependaboard_core::MAX_RECENT_BATCHES))
        .await
        .map_err(store_failure)
}

#[server(state: Extension<ServerState>)]
pub(crate) async fn load_pr_status(
    repository_id: u64,
    number: u64,
) -> Result<Option<PrState>, ServerFnError> {
    state
        .ingress
        .call(&pr_status_path(repository_id, number))
        .await
        .map_err(restate_unavailable)
}

#[server(state: Extension<ServerState>)]
pub(crate) async fn load_pr_projection(
    repository_id: u64,
    number: u64,
) -> Result<Option<PrRecord>, ServerFnError> {
    state
        .store
        .get_pr(&PrKey::new(repository_id, number))
        .await
        .map_err(store_failure)
}

#[server(state: Extension<ServerState>)]
pub(crate) async fn request_sync() -> Result<(), ServerFnError> {
    state
        .ingress
        .send_empty("DashboardIngress/sync_installation")
        .await
        .map_err(restate_unavailable)
}

#[server(state: Extension<ServerState>)]
pub(crate) async fn request_pr_sync(
    repository_id: u64,
    number: u64,
) -> Result<String, ServerFnError> {
    let row = projected_pr(&state, &PrKey::new(repository_id, number)).await?;
    let completion_id = new_batch_id();
    let request = ManualSyncRequest {
        repository_id: row.repository_id,
        owner: row.owner,
        repo: row.repo,
        number: row.number,
        completion_id: completion_id.clone(),
    };
    state
        .ingress
        .send("DashboardIngress/sync_pull_request", &request, None)
        .await
        .map_err(restate_unavailable)?;
    Ok(completion_id)
}

/// The pull request the browser named, as the projection has it. A key the
/// dashboard does not know is refused, as is one whose repository belongs to
/// another installation: the browser only ever names a key, and the
/// projection's row is the word on which installation the pull request is
/// in and what it is.
#[cfg(feature = "server")]
async fn projected_pr(state: &ServerState, key: &PrKey) -> Result<PrRecord, ServerFnError> {
    let row = state
        .store
        .get_pr(key)
        .await
        .map_err(store_failure)?
        .ok_or_else(|| {
            ServerFnError::new(format!(
                "pull request #{} is no longer in the dashboard",
                key.number
            ))
        })?;
    if row.installation_id != state.installation_id {
        return Err(ServerFnError::new(format!(
            "pull request #{} does not belong to the configured installation",
            key.number
        )));
    }
    Ok(row)
}

/// Turns a read-model failure into the browser's error. A page cursor the
/// browser sent that no longer parses is its own fault and is named as such;
/// anything else is logged in full and reduced to a message that names the
/// component, not the cause: the cause can carry connection strings and
/// credentials, and the user cannot act on it anyway.
#[cfg(feature = "server")]
fn store_failure(error: StoreError) -> ServerFnError {
    match error {
        StoreError::Cursor(error) => ServerFnError::new(error.to_string()),
        error => {
            tracing::error!(%error, "read model query failed");
            ServerFnError::new("The read model is unavailable")
        }
    }
}

/// Logs a Restate failure in full; the browser learns only that Restate did
/// not take the request.
#[cfg(feature = "server")]
fn restate_unavailable(error: String) -> ServerFnError {
    tracing::error!(error, "Restate request failed");
    ServerFnError::new("Restate is unavailable")
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::CursorError;
    use reqwest::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::server::test_support::{
        Dashboard, INSTALLATION_ID, USERNAME, dashboard, error_message,
    };
    use crate::ui::test_support::{GROUPED_ROW_TITLE, grouped_row, serde_row};

    #[test]
    fn infrastructure_failures_reach_the_browser_without_their_detail() {
        let detail = "libsql://db.internal: connection refused (token=abc)";
        let store = store_failure(StoreError::CorruptEnum(detail.to_owned()));
        let restate = restate_unavailable(format!("Restate returned 502: {detail}"));

        for error in [store, restate] {
            let ServerFnError::ServerError { message, .. } = error else {
                panic!("a server-side failure: {error}");
            };
            assert!(!message.contains(detail), "{message}");
            assert!(!message.contains("libsql"), "{message}");
            assert!(!message.is_empty());
        }
    }

    /// A cursor the browser sent is the browser's fault, not the store's, and
    /// saying so is what lets the user start over.
    #[test]
    fn a_bad_page_cursor_is_reported_as_such() {
        let ServerFnError::ServerError { message, .. } =
            store_failure(StoreError::Cursor(CursorError::Invalid))
        else {
            panic!("a server-side failure");
        };

        assert_eq!(message, "invalid page cursor");
    }

    /// The body of the one request Restate was sent, which went to `path`.
    fn the_one_request(dashboard: &Dashboard, path: &str) -> serde_json::Value {
        serde_json::from_slice(&dashboard.the_one_forward(path).body).unwrap()
    }

    /// A batch target as the browser submits it.
    fn target(repository_id: u64, number: u64, expected_sha: &str) -> serde_json::Value {
        json!({ "repository_id": repository_id, "number": number, "expected_sha": expected_sha })
    }

    /// Submits a merge batch of `targets` under `batch_id`.
    async fn submit(
        dashboard: &Dashboard,
        batch_id: &str,
        targets: serde_json::Value,
    ) -> reqwest::Response {
        dashboard
            .call(
                "submit_batch",
                json!({ "batch_id": batch_id, "action": "merge", "targets": targets }),
            )
            .send()
            .await
            .unwrap()
    }

    /// A batch names its targets by key and the head the user saw; the rest
    /// of what the workflow and the audit record say about a target is the
    /// projection's word, whatever the browser sent along.
    #[tokio::test]
    async fn a_batch_carries_the_projections_word_on_its_targets_and_the_browsers_on_the_head() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        let batch_id = new_batch_id();
        let mut forged = target(7, 9, "before-the-push");
        forged["title"] = json!("Click here");
        forged["html_url"] = json!("https://evil.example/");

        let response = submit(&dashboard, &batch_id, json!([forged])).await;

        assert_eq!(response.status(), StatusCode::OK);
        let forward =
            dashboard.the_one_forward(&format!("/restate/send/BulkAction/{batch_id}/run"));
        assert_eq!(forward.idempotency_key.as_deref(), Some(batch_id.as_str()));
        let request: serde_json::Value = serde_json::from_slice(&forward.body).unwrap();
        assert_eq!(
            request,
            json!({
                "action": "merge",
                "user_id": USERNAME,
                "targets": [{
                    "repository_id": 7,
                    "owner": "acme",
                    "repo": "api",
                    "number": 9,
                    "expected_sha": "before-the-push",
                    "title": GROUPED_ROW_TITLE,
                    "html_url": "https://github.example/acme/api/pull/9",
                }],
            })
        );
    }

    /// One target the dashboard cannot vouch for refuses the whole batch: a
    /// pull request from another installation, or one the projection no
    /// longer has. Nothing reaches Restate for a batch that was refused.
    #[tokio::test]
    async fn a_batch_with_a_target_the_dashboard_cannot_vouch_for_is_refused_whole() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        dashboard.project(INSTALLATION_ID + 1, &serde_row()).await;
        let own = target(7, 9, "abc123");

        let foreign = submit(
            &dashboard,
            &new_batch_id(),
            json!([own, target(8, 12, "def456")]),
        )
        .await;
        assert_eq!(foreign.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error_message(foreign).await,
            "pull request #12 does not belong to the configured installation"
        );

        let gone = submit(
            &dashboard,
            &new_batch_id(),
            json!([own, target(7, 10, "abc124")]),
        )
        .await;
        assert_eq!(gone.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error_message(gone).await,
            "pull request #10 is no longer in the dashboard"
        );

        assert!(dashboard.forwards().is_empty());
    }

    /// The batch rules come first: a submission that could not run as a
    /// batch is refused as such, before a single target is looked up, so an
    /// unknown target in it goes unmentioned.
    #[tokio::test]
    async fn a_submission_is_held_to_the_batch_rules_before_its_targets_are_resolved() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        let own = target(7, 9, "abc123");
        let unknown = target(7, 10, "abc124");

        let bad_id = submit(
            &dashboard,
            "550e8400-e29b-41d4-a716-446655440000",
            json!([unknown]),
        )
        .await;
        assert_eq!(error_message(bad_id).await, "batch id must be a UUIDv7");

        let empty = submit(&dashboard, &new_batch_id(), json!([])).await;
        assert_eq!(
            error_message(empty).await,
            "batch must contain between 1 and 100 targets"
        );

        let repeated = submit(&dashboard, &new_batch_id(), json!([own, unknown, own])).await;
        assert_eq!(
            error_message(repeated).await,
            "batch contains duplicate pull requests"
        );

        assert!(dashboard.forwards().is_empty());
    }

    /// The browser names a pull request by key; the projection says which
    /// repository it is in and whose it is. One the dashboard does not know,
    /// or one from another installation, is refused before Restate hears of
    /// it, and the sync request that does go out names the repository as the
    /// projection has it.
    #[tokio::test]
    async fn a_pull_request_sync_is_resolved_from_the_projection_and_held_to_the_installation() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        dashboard.project(INSTALLATION_ID + 1, &serde_row()).await;
        let sync = |row: PrRecord| {
            dashboard
                .call(
                    "request_pr_sync",
                    json!({ "repository_id": row.repository_id, "number": row.number }),
                )
                .send()
        };

        let unknown = sync(PrRecord {
            number: 99,
            ..grouped_row()
        })
        .await
        .unwrap();
        assert_eq!(unknown.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error_message(unknown).await,
            "pull request #99 is no longer in the dashboard"
        );

        let foreign = sync(serde_row()).await.unwrap();
        assert_eq!(foreign.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error_message(foreign).await,
            "pull request #12 does not belong to the configured installation"
        );
        assert!(dashboard.forwards().is_empty());

        let own = sync(grouped_row()).await.unwrap();
        assert_eq!(own.status(), StatusCode::OK);
        let completion_id: String = own.json().await.unwrap();
        let request = the_one_request(
            &dashboard,
            "/restate/send/DashboardIngress/sync_pull_request",
        );
        assert_eq!(
            request,
            json!({
                "repository_id": 7,
                "owner": "acme",
                "repo": "api",
                "number": 9,
                "completion_id": completion_id,
            })
        );
    }
}
