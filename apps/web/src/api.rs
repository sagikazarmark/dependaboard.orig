//! The server functions the dashboard calls. Compiled on both targets: the
//! wasm build gets the client stubs, the server build the bodies, which reach
//! the store and Restate through [`crate::server`].

use dependaboard_core::{
    BatchProgress, BulkActionKind, DashboardPage, Page, PrFilter, PrRecord, PrState, PrTarget,
};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use {
    crate::server::{
        installation::github_installation_id,
        restate::{pr_status_path, restate_call, restate_send, restate_send_empty},
        store::store,
    },
    axum::extract::Extension,
    dependaboard_core::{BulkRequest, ManualSyncRequest, PrKey, UserId, new_batch_id},
    dependaboard_store::PrStore,
    std::collections::BTreeSet,
};

#[server]
pub(crate) async fn load_dashboard(
    filter: PrFilter,
    page: Page,
) -> Result<DashboardPage, ServerFnError> {
    let store = store().await?;
    store
        .list_prs(&filter, page)
        .await
        .map_err(|error| ServerFnError::new(error.to_string()))
}

#[server(user: Extension<UserId>)]
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
    restate_send(&format!("BulkAction/{batch_id}/run"), &request)
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub(crate) async fn load_batch_progress(
    batch_id: String,
) -> Result<Option<BatchProgress>, ServerFnError> {
    if !dependaboard_core::valid_batch_id(&batch_id) {
        return Err(ServerFnError::new("batch id must be a UUIDv7"));
    }
    restate_call(&format!("BulkAction/{batch_id}/progress"))
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub(crate) async fn load_pr_status(
    repository_id: u64,
    number: u64,
) -> Result<Option<PrState>, ServerFnError> {
    restate_call(&pr_status_path(repository_id, number))
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub(crate) async fn load_pr_projection(
    repository_id: u64,
    number: u64,
) -> Result<Option<PrRecord>, ServerFnError> {
    store()
        .await?
        .get_pr(&PrKey::new(repository_id, number))
        .await
        .map_err(|error| ServerFnError::new(error.to_string()))
}

#[server]
pub(crate) async fn request_sync() -> Result<(), ServerFnError> {
    restate_send_empty("DashboardIngress/sync_installation")
        .await
        .map_err(ServerFnError::new)
}

#[server]
pub(crate) async fn request_pr_sync(
    repository_id: u64,
    number: u64,
) -> Result<String, ServerFnError> {
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
    let request = ManualSyncRequest {
        repository_id: row.repository_id,
        owner: row.owner,
        repo: row.repo,
        number: row.number,
        completion_id: completion_id.clone(),
    };
    restate_send("DashboardIngress/sync_pull_request", &request)
        .await
        .map_err(ServerFnError::new)?;
    Ok(completion_id)
}
