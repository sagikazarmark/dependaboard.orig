//! The server functions the dashboard calls. Compiled on both targets: the
//! wasm build gets the client stubs, the server build the bodies, which reach
//! the store and Restate through [`crate::server::state::ServerState`].

use dependaboard_core::{
    BatchProgress, BatchRecord, BulkActionKind, DashboardPage, DashboardSummary, Page, PrFilter,
    PrRecord, PrState, PrTarget,
};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use {
    crate::server::{restate::pr_status_path, state::ServerState},
    axum::extract::Extension,
    dependaboard_core::{BulkRequest, ManualSyncRequest, PrKey, UserId, new_batch_id},
    dependaboard_store::{PrStore, StoreError},
    std::collections::BTreeSet,
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

/// The read model's revision: a counter that moves whenever a row changes.
/// Cheap enough for the dashboard to poll, so it can reload the rows only
/// when the answer has moved since it last asked.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_projection_revision() -> Result<u64, ServerFnError> {
    state
        .store
        .projection_revision()
        .await
        .map_err(store_failure)
}

#[server(state: Extension<ServerState>, user: Extension<UserId>)]
pub(crate) async fn submit_batch(
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
    state
        .ingress
        .send(&format!("BulkAction/{batch_id}/run"), &request, None)
        .await
        .map_err(restate_unavailable)
}

#[server(state: Extension<ServerState>)]
pub(crate) async fn load_batch_progress(
    batch_id: String,
) -> Result<Option<BatchProgress>, ServerFnError> {
    if !dependaboard_core::valid_batch_id(&batch_id) {
        return Err(ServerFnError::new("batch id must be a UUIDv7"));
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
    let row = state
        .store
        .get_pr(&PrKey::new(repository_id, number))
        .await
        .map_err(store_failure)?
        .ok_or_else(|| ServerFnError::new("pull request is no longer in the dashboard"))?;
    if row.installation_id != state.installation_id {
        return Err(ServerFnError::new(
            "pull request does not belong to the configured installation",
        ));
    }
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

    use super::*;

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
}
