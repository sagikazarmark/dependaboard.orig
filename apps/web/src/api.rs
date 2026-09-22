//! The server functions the dashboard calls. Compiled on both targets: the
//! wasm build gets the client stubs, the server build the bodies, which reach
//! the store and Restate through [`crate::server::state::ServerState`].
//!
//! Each `#[server]` function with a body is a one-line adapter over a `*_in`
//! body that takes the state — and, where it matters, the user — as
//! arguments, so the body is called in a test without a server, a signed-in
//! request or the wire's error envelope between the test and what it
//! asserts. The one exception, [`load_signed_in_user`], has no body: it
//! answers with what the auth edge extracted.
//!
//! A body refuses with an [`ApiError`] rather than the wire's envelope, and
//! the envelope is minted from it in one place — the `From<ApiError>` the
//! `#[server]` adapters apply with `?`. That is where an infrastructure
//! failure's detail is dropped, which is the only place it can be dropped
//! from, and where the status every refusal carries is decided, which is the
//! only place the browser's reading of a refusal can be changed.

#[cfg(feature = "server")]
use std::fmt;

use dependaboard_core::{
    BatchList, BatchProgress, BatchReceipt, BulkActionKind, Capabilities, DashboardPage,
    DashboardSummary, Page, PrFilter, PrRecord, PrState, ProjectedBatch, ProjectionRevision,
    SubmittedTarget, UserId,
};
#[cfg(feature = "server")]
use dependaboard_core::{CursorError, InvalidBatch, PrKey};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use {
    crate::server::{
        restate::{RestateIngressError, pr_status_path},
        state::ServerState,
    },
    axum::extract::Extension,
    dependaboard_core::{BulkRequest, ManualSyncRequest, PrTarget, new_batch_id, validate_batch},
    dependaboard_store::StoreError,
    reqwest::StatusCode,
};

/// What a server function's body refuses with.
///
/// Two kinds of refusal, kept apart because the user is told different
/// amounts. A request the dashboard should not have sent is named in full:
/// the message is the whole of what was wrong with it, and it is the user's
/// or the developer's to act on. A piece of the deployment that did not
/// answer is named and no more — the failure behind it can carry connection
/// strings and the credentials in them, so it rides along for the log alone
/// and goes no further than the conversion at the wire's edge.
#[cfg(feature = "server")]
#[derive(Debug)]
pub(crate) enum ApiError {
    /// A request no dashboard of this deployment would have sent, or one the
    /// projection can no longer make sense of.
    InvalidRequest(InvalidRequest),
    /// `component` could not be reached, or would not answer; `cause` is what
    /// it failed with, for the log and never for the browser.
    Unavailable {
        component: Upstream,
        cause: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// A request the dashboard should not have sent, as the body that refused it
/// saw it. Each variant carries what its message varies by and nothing else,
/// so a test can assert the refusal rather than the sentence.
#[cfg(feature = "server")]
#[derive(Debug)]
pub(crate) enum InvalidRequest {
    /// A submission that could not be a batch, by the batch rules.
    Batch(InvalidBatch),
    /// A page cursor the browser sent that no longer parses.
    Cursor(CursorError),
    /// A pull request of another installation: the projection never showed
    /// it, so no dashboard of this deployment could have named it.
    ForeignInstallation(PrKey),
    /// A pull request the projection no longer has, asked for by a request
    /// that has nothing to do without it.
    NoLongerInTheDashboard(PrKey),
    /// A submission none of whose `submitted` targets the projection still
    /// has: no batch left to run.
    NothingLeftToRun { submitted: usize },
}

/// A part of the deployment the dashboard cannot answer without. Named
/// `Upstream` rather than the obvious word, which is Dioxus's.
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Upstream {
    /// The store every dashboard read is served from.
    ReadModel,
    /// The ingress every action is enqueued on.
    Restate,
}

#[cfg(feature = "server")]
impl Upstream {
    /// What the user is told when this component does not answer — all of
    /// it. An action is not withheld while the line is down; it is offered
    /// and fails with its own message (README §Live refresh), so these two
    /// sentences are what the dashboard shows in place of an answer, and the
    /// tests of the controls that show them read them back word for word.
    const fn unavailable(self) -> &'static str {
        match self {
            Self::ReadModel => "The read model is unavailable",
            Self::Restate => "Restate is unavailable",
        }
    }
}

#[cfg(feature = "server")]
impl fmt::Display for ApiError {
    /// The whole of what the browser is shown. The `cause` of an
    /// [`ApiError::Unavailable`] is deliberately absent: redaction is this
    /// impl, not a rule each refusal has to remember.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(invalid) => invalid.fmt(f),
            Self::Unavailable { component, .. } => f.write_str(component.unavailable()),
        }
    }
}

#[cfg(feature = "server")]
impl std::error::Error for ApiError {
    /// The failure behind an [`ApiError::Unavailable`], which the log is
    /// given whole. An [`ApiError::InvalidRequest`] has nothing beneath its
    /// message: the message is the whole of what happened.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRequest(_) => None,
            Self::Unavailable { cause, .. } => Some(cause.as_ref()),
        }
    }
}

#[cfg(feature = "server")]
impl fmt::Display for InvalidRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Batch(invalid) => invalid.fmt(f),
            Self::Cursor(invalid) => invalid.fmt(f),
            Self::ForeignInstallation(key) => write!(
                f,
                "pull request #{} does not belong to the configured installation",
                key.number
            ),
            Self::NoLongerInTheDashboard(key) => {
                write!(
                    f,
                    "pull request #{} is no longer in the dashboard",
                    key.number
                )
            }
            Self::NothingLeftToRun { submitted } => write!(
                f,
                "none of the {submitted} pull requests submitted are still in the dashboard"
            ),
        }
    }
}

#[cfg(feature = "server")]
impl std::error::Error for InvalidRequest {}

#[cfg(feature = "server")]
impl From<InvalidRequest> for ApiError {
    fn from(invalid: InvalidRequest) -> Self {
        Self::InvalidRequest(invalid)
    }
}

/// A read-model failure. A page cursor the browser sent that no longer parses
/// is the browser's fault and is named as such; every other failure is the
/// read model not answering.
#[cfg(feature = "server")]
impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Cursor(invalid) => InvalidRequest::Cursor(invalid).into(),
            error => Self::Unavailable {
                component: Upstream::ReadModel,
                cause: Box::new(error),
            },
        }
    }
}

/// Restate not taking the request. Whether it refused, timed out or was never
/// reached is the log's business; a refusal that says something about the
/// request rather than about Restate — the 404 that is a batch it never had —
/// is read before this conversion, by the body that asked.
#[cfg(feature = "server")]
impl From<RestateIngressError> for ApiError {
    fn from(error: RestateIngressError) -> Self {
        Self::Unavailable {
            component: Upstream::Restate,
            cause: Box::new(error),
        }
    }
}

/// The one place a refusal becomes the wire's envelope, and so the one place
/// the browser's reading of it is decided.
///
/// Every refusal is a 500 carrying a message meant for the user, because
/// [`crate::ui::fault`] reads the status and nothing else: only a 500 is
/// shown as the server's own word, and a refusal answered with any other
/// status would reach the user as "the dashboard server could not be
/// reached" instead of what it says. This is also where an unreachable
/// component's failure is logged and dropped.
#[cfg(feature = "server")]
impl From<ApiError> for ServerFnError {
    fn from(error: ApiError) -> Self {
        if let ApiError::Unavailable { component, cause } = &error {
            tracing::error!(?component, %cause, "a dependency of the dashboard did not answer");
        }
        ServerFnError::new(error)
    }
}

/// One page of rows for `filter`. Paging through a filter calls this alone;
/// the facets around the rows come from [`load_summary`], once per filter.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_dashboard(
    filter: PrFilter,
    page: Page,
) -> Result<DashboardPage, ServerFnError> {
    Ok(load_dashboard_in(&state, &filter, page).await?)
}

/// The body of [`load_dashboard`].
#[cfg(feature = "server")]
pub(crate) async fn load_dashboard_in(
    state: &ServerState,
    filter: &PrFilter,
    page: Page,
) -> Result<DashboardPage, ApiError> {
    Ok(state.store.list_prs(filter, page).await?)
}

/// The facet counts scoped to `filter` and the read model's freshness.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_summary(filter: PrFilter) -> Result<DashboardSummary, ServerFnError> {
    Ok(load_summary_in(&state, &filter).await?)
}

/// The body of [`load_summary`].
#[cfg(feature = "server")]
pub(crate) async fn load_summary_in(
    state: &ServerState,
    filter: &PrFilter,
) -> Result<DashboardSummary, ApiError> {
    Ok(state.store.dashboard_summary(filter).await?)
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
    Ok(load_matching_in(&state, &filter).await?)
}

/// The body of [`load_matching`].
#[cfg(feature = "server")]
pub(crate) async fn load_matching_in(
    state: &ServerState,
    filter: &PrFilter,
) -> Result<DashboardPage, ApiError> {
    let batch = Page {
        limit: u32::try_from(dependaboard_core::MAX_BATCH_TARGETS)
            .expect("the batch limit fits in a page"),
        after: None,
    };
    Ok(state.store.list_prs(filter, batch).await?)
}

/// The read model's revision: a counter that moves whenever a row changes,
/// beside one that moves only when a pull request row does. Cheap enough for
/// the dashboard to poll, so it can reload the rows only when the answer has
/// moved since it last asked, and tell a sweep's first repository write from
/// its pull requests landing.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_projection_revision() -> Result<ProjectionRevision, ServerFnError> {
    Ok(load_projection_revision_in(&state).await?)
}

/// The body of [`load_projection_revision`].
#[cfg(feature = "server")]
pub(crate) async fn load_projection_revision_in(
    state: &ServerState,
) -> Result<ProjectionRevision, ApiError> {
    Ok(state.store.projection_revision().await?)
}

/// Who the auth edge let this request through as. The browser holds the
/// credentials but cannot read them, so this is how the dashboard learns
/// whose name to show; the answer is the identity a batch will be recorded
/// under. There is no body to call without a server: the answer is what the
/// edge extracted, and the test of it is the 401 the edge answers a stale
/// credential with.
#[server(user: Extension<UserId>)]
pub(crate) async fn load_signed_in_user() -> Result<UserId, ServerFnError> {
    Ok(user.0)
}

/// What the Restate service can do, as it resolved at startup: which actions
/// the dashboard should offer. Asked of the service rather than read from
/// this process's environment, since the service is the one that holds the
/// credentials the answer turns on.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_capabilities() -> Result<Capabilities, ServerFnError> {
    Ok(load_capabilities_in(&state).await?)
}

/// The body of [`load_capabilities`].
#[cfg(feature = "server")]
pub(crate) async fn load_capabilities_in(state: &ServerState) -> Result<Capabilities, ApiError> {
    Ok(state.ingress.call("DashboardIngress/capabilities").await?)
}

/// Asks Restate to run the batch. The batch id is the workflow key, which lets
/// a workflow run once, and the idempotency key, which lets the browser resend
/// the same submission after a lost response and be told it was accepted
/// rather than refused for the workflow already existing.
///
/// The browser names each target by key and the head it saw, and the batch
/// this one retries when it is a retry; the rest of each target is resolved
/// here from the projection. A target the projection no longer has — merged or
/// closed between the selection and the click — is one pull request's
/// business, not the batch's: it is left out, named by key in the receipt, and
/// the batch runs over the rest. A target from another installation is no race
/// but a client that should not exist, and refuses the batch whole; so does a
/// submission that leaves nothing to run. The submission is held to the batch
/// rules first, so one that could not run resolves nothing. The batch retried
/// is taken on the browser's word, as the targets are: held to the shape of a
/// batch id, not looked up.
#[server(state: Extension<ServerState>, user: Extension<UserId>)]
pub(crate) async fn submit_batch(
    batch_id: String,
    action: BulkActionKind,
    targets: Vec<SubmittedTarget>,
    retried_from: Option<String>,
) -> Result<BatchReceipt, ServerFnError> {
    Ok(submit_batch_in(&state, user.0, &batch_id, action, targets, retried_from).await?)
}

/// The body of [`submit_batch`], for `user`, the identity the auth edge let
/// the request through as.
#[cfg(feature = "server")]
pub(crate) async fn submit_batch_in(
    state: &ServerState,
    user: UserId,
    batch_id: &str,
    action: BulkActionKind,
    targets: Vec<SubmittedTarget>,
    retried_from: Option<String>,
) -> Result<BatchReceipt, ApiError> {
    validate_batch(
        batch_id,
        retried_from.as_deref(),
        targets.iter().map(SubmittedTarget::key),
    )
    .map_err(InvalidRequest::Batch)?;
    let submitted = targets.len();
    let mut resolved = Vec::with_capacity(submitted);
    let mut left_out = Vec::new();
    for target in targets {
        let key = target.key();
        let Some(row) = projected_pr(state, &key).await? else {
            left_out.push(key);
            continue;
        };
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
    if resolved.is_empty() {
        return Err(InvalidRequest::NothingLeftToRun { submitted }.into());
    }
    let request = BulkRequest {
        action,
        targets: resolved,
        user_id: user,
        retried_from,
    };
    state
        .ingress
        .send(
            &format!("BulkAction/{batch_id}/run"),
            &request,
            Some(batch_id),
        )
        .await?;
    Ok(BatchReceipt { left_out })
}

/// Where the batch stands, as Restate holds it, or `None` for a workflow this
/// deployment's Restate has never heard of. Not held to the projection, as the
/// other batch reads are: each deployment has its own Restate, so another
/// installation's batch is unknown here whatever the shared store holds, a
/// UUIDv7 is not to be enumerated, and a batch just queued is polled before
/// the workflow's first step has listed it.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_batch_progress(
    batch_id: String,
) -> Result<Option<BatchProgress>, ServerFnError> {
    Ok(load_batch_progress_in(&state, &batch_id).await?)
}

/// The body of [`load_batch_progress`]. Restate answers a shared handler of a
/// workflow it never had with a 404, and that is the `None`: the one refusal
/// that says something about the batch rather than about Restate, and the
/// one the follow gives up on. Every other failure is Restate not answering
/// for the batch, and is reported as such, so the follow waits it out.
#[cfg(feature = "server")]
pub(crate) async fn load_batch_progress_in(
    state: &ServerState,
    batch_id: &str,
) -> Result<Option<BatchProgress>, ApiError> {
    minted_here(batch_id)?;
    match state
        .ingress
        .call(&format!("BulkAction/{batch_id}/progress"))
        .await
    {
        Ok(progress) => Ok(progress),
        Err(RestateIngressError::Status {
            code: StatusCode::NOT_FOUND,
            ..
        }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The batches the audit view lists: every batch running, and the `limit`
/// most recently finished, newest first, as the projection keeps them: what
/// was asked, by whom, when, and — once finished — how each target went. Read
/// from the store, not Restate, so a finished batch is still here after the
/// workflow's retention has cleared its progress, and a running one is found
/// without knowing its id. The configured installation's batches only: two
/// deployments sharing a store do not list each other's. `limit` is held to
/// [`MAX_RECENT_BATCHES`](dependaboard_core::MAX_RECENT_BATCHES).
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_recent_batches(limit: u32) -> Result<BatchList, ServerFnError> {
    Ok(load_recent_batches_in(&state, limit).await?)
}

/// The body of [`load_recent_batches`].
#[cfg(feature = "server")]
pub(crate) async fn load_recent_batches_in(
    state: &ServerState,
    limit: u32,
) -> Result<BatchList, ApiError> {
    let running = state.store.running_batches(state.installation_id).await?;
    let finished = state
        .store
        .recent_batches(
            state.installation_id,
            limit.clamp(1, dependaboard_core::MAX_RECENT_BATCHES),
        )
        .await?;
    Ok(BatchList { running, finished })
}

/// What the projection holds of the batch `batch_id` names: its finished
/// record, its listing while it runs, or `None` for an id it has never heard
/// of. A dashboard handed an id alone — from a link — asks this before it
/// asks Restate, so a finished batch opens from the record at once, however
/// long ago its workflow was retired, and only an id the projection has
/// never heard of is asked after through `progress`. Held to the configured
/// installation: another installation's batch is answered as one the
/// projection has never heard of, not refused, since a link to it is given up
/// the same way a stale one is. An id the dashboard could not have minted is
/// refused, as [`load_batch_progress`] refuses it.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_batch_projection(
    batch_id: String,
) -> Result<Option<ProjectedBatch>, ServerFnError> {
    Ok(load_batch_projection_in(&state, &batch_id).await?)
}

/// The body of [`load_batch_projection`].
#[cfg(feature = "server")]
pub(crate) async fn load_batch_projection_in(
    state: &ServerState,
    batch_id: &str,
) -> Result<Option<ProjectedBatch>, ApiError> {
    minted_here(batch_id)?;
    Ok(state
        .store
        .get_batch(state.installation_id, batch_id)
        .await?)
}

/// The durable state Restate holds for a pull request, or `None` for one the
/// projection does not have: the `PullRequest` object knows nothing of
/// installations, so the key is held to the projection before Restate is
/// asked, and a key with no row is answered with no state, which is how the
/// drawer learns the pull request is no longer open.
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_pr_status(
    repository_id: u64,
    number: u64,
) -> Result<Option<PrState>, ServerFnError> {
    Ok(load_pr_status_in(&state, PrKey::new(repository_id, number)).await?)
}

/// The body of [`load_pr_status`].
#[cfg(feature = "server")]
pub(crate) async fn load_pr_status_in(
    state: &ServerState,
    key: PrKey,
) -> Result<Option<PrState>, ApiError> {
    if projected_pr(state, &key).await?.is_none() {
        return Ok(None);
    }
    Ok(state
        .ingress
        .call(&pr_status_path(key.repository_id, key.number))
        .await?)
}

/// The pull request's row as the projection has it, or `None` for one the
/// projection does not: what the drawer opens on, and reads back after a
/// sync. Held to the installation like every other read of a key the browser
/// names; see [`projected_pr`].
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_pr_projection(
    repository_id: u64,
    number: u64,
) -> Result<Option<PrRecord>, ServerFnError> {
    Ok(load_pr_projection_in(&state, PrKey::new(repository_id, number)).await?)
}

/// The body of [`load_pr_projection`]: [`projected_pr`] for the key the
/// browser named, and nothing else, which makes it where that resolution is
/// tested.
#[cfg(feature = "server")]
pub(crate) async fn load_pr_projection_in(
    state: &ServerState,
    key: PrKey,
) -> Result<Option<PrRecord>, ApiError> {
    projected_pr(state, &key).await
}

/// Asks Restate to reconcile the whole installation now, without waiting for
/// the scheduler's next sweep: what the **Sync** control queues. One-way:
/// Restate takes it, and the dashboard's live refresh sees the sweep land.
#[server(state: Extension<ServerState>)]
pub(crate) async fn request_sync() -> Result<(), ServerFnError> {
    Ok(request_sync_in(&state).await?)
}

/// The body of [`request_sync`].
#[cfg(feature = "server")]
pub(crate) async fn request_sync_in(state: &ServerState) -> Result<(), ApiError> {
    Ok(state
        .ingress
        .send_empty("DashboardIngress/sync_installation")
        .await?)
}

/// Asks Restate to refresh one pull request, and answers with the completion
/// id the refresh will be recorded under, which is how the drawer tells this
/// refresh from the webhook syncs around it. The browser names the pull
/// request by key; the projection's row says which repository it is in, and
/// a key the projection has no row for is refused, since a sync of it has
/// nothing to refresh.
#[server(state: Extension<ServerState>)]
pub(crate) async fn request_pr_sync(
    repository_id: u64,
    number: u64,
) -> Result<String, ServerFnError> {
    Ok(request_pr_sync_in(&state, PrKey::new(repository_id, number)).await?)
}

/// The body of [`request_pr_sync`].
#[cfg(feature = "server")]
pub(crate) async fn request_pr_sync_in(
    state: &ServerState,
    key: PrKey,
) -> Result<String, ApiError> {
    let row = projected_pr(state, &key)
        .await?
        .ok_or_else(|| InvalidRequest::NoLongerInTheDashboard(key.clone()))?;
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
        .await?;
    Ok(completion_id)
}

/// The pull request the browser named, as the projection has it, or `None`
/// for one the projection does not: the browser only ever names a key, and
/// the projection's row is the word on which installation the pull request
/// is in and what it is. A key whose repository belongs to another
/// installation is refused outright rather than answered with nothing: the
/// projection never showed it, so no dashboard could have named it.
///
/// Every server function that takes a key from the browser — a read as much
/// as a sync or a batch target — resolves it here, so the installation is
/// held to in one place rather than remembered at each.
#[cfg(feature = "server")]
async fn projected_pr(state: &ServerState, key: &PrKey) -> Result<Option<PrRecord>, ApiError> {
    let Some(row) = state.store.get_pr(key).await? else {
        return Ok(None);
    };
    if row.installation_id != state.installation_id {
        return Err(InvalidRequest::ForeignInstallation(key.clone()).into());
    }
    Ok(Some(row))
}

/// Holds a batch id the browser named to the shape this deployment mints,
/// for the two reads that take one. Neither looks the id up — an id the
/// projection has never heard of is an answer, not a refusal — so the shape
/// is the whole of what they can hold a batch id to, and they hold it to the
/// same shape in this one place.
#[cfg(feature = "server")]
fn minted_here(batch_id: &str) -> Result<(), ApiError> {
    if dependaboard_core::valid_batch_id(batch_id) {
        Ok(())
    } else {
        Err(InvalidRequest::Batch(InvalidBatch::BatchId).into())
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::error::Error as _;

    use dependaboard_core::{
        BatchRecord, BatchTargetRecord, ProjectedBatch, RunningBatch, TargetOutcome,
    };
    use serde_json::json;

    // The tests write to the projection as the Restate service would; the
    // server functions under test only ever read it.
    use dependaboard_store::ProjectionWriter;

    use super::*;
    use crate::server::test_support::{
        Backend, Dashboard, INSTALLATION_ID, USERNAME, backend, dashboard, error_message,
    };
    use crate::ui::test_support::{BATCH, GROUPED_ROW_TITLE, OTHER_BATCH, grouped_row, serde_row};

    /// A well-formed id no batch has ever had.
    const UNKNOWN_BATCH: &str = "01926e3a-7c1e-7b7d-9f8b-2b4c6d8e0f1c";

    /// A running merge of three pull requests as the workflow serving
    /// `installation_id` lists it.
    fn listed(installation_id: u64, batch_id: &str) -> RunningBatch {
        RunningBatch {
            batch_id: batch_id.to_owned(),
            installation_id,
            action: BulkActionKind::Merge,
            requested_by: UserId::new(USERNAME),
            retried_from: None,
            started_at: 2_000,
            target_count: 3,
        }
    }

    /// A finished one-target rebase as the workflow serving `installation_id`
    /// records it.
    fn recorded(installation_id: u64, batch_id: &str) -> BatchRecord {
        BatchRecord {
            batch_id: batch_id.to_owned(),
            installation_id,
            action: BulkActionKind::Rebase,
            requested_by: UserId::new(USERNAME),
            retried_from: None,
            started_at: 1_000,
            completed_at: 1_030,
            succeeded: 1,
            rejected: 0,
            failed: 0,
            targets: vec![BatchTargetRecord {
                repository_id: 7,
                owner: "acme".to_owned(),
                repo: "api".to_owned(),
                number: 9,
                title: GROUPED_ROW_TITLE.to_owned(),
                html_url: "https://github.example/acme/api/pull/9".to_owned(),
                head_sha: Some("abc123".to_owned()),
                outcome: TargetOutcome::Succeeded {
                    detail: "@dependabot rebase posted".to_owned(),
                    merge_sha: None,
                },
            }],
        }
    }

    /// The message `error` reaches the browser as: what the one conversion
    /// at the wire's edge puts in a failed server function's envelope, and
    /// the user is shown.
    fn reported(error: ApiError) -> String {
        match ServerFnError::from(error) {
            ServerFnError::ServerError { message, .. } => message,
            other => panic!("a server-side failure: {other}"),
        }
    }

    /// The message a body refused with; the one the browser would be shown.
    fn refusal<T: std::fmt::Debug>(result: Result<T, ApiError>) -> String {
        reported(result.expect_err("a refusal"))
    }

    /// A store or a Restate failure can name the connection it failed on,
    /// and the credentials in it. The operator gets it whole — there is
    /// nothing else to debug an outage with — and the browser gets the
    /// component's name and not one word more.
    ///
    /// Both halves are asserted, and the first is what keeps the second
    /// honest: the failure must still be *carried* as far as the conversion,
    /// so that dropping it there is a decision the conversion makes rather
    /// than something the refusal never had. An [`ApiError`] that stopped
    /// carrying its cause would redact nothing and still pass a
    /// browser-side assertion alone.
    #[test]
    fn infrastructure_failures_reach_the_browser_without_their_detail() {
        let detail = "libsql://db.internal: connection refused (token=abc)";
        let store = ApiError::from(StoreError::CorruptEnum(detail.to_owned()));
        let restate = ApiError::from(RestateIngressError::Status {
            code: StatusCode::BAD_GATEWAY,
            body: detail.to_owned(),
        });

        for (error, told) in [
            (store, "The read model is unavailable"),
            (restate, "Restate is unavailable"),
        ] {
            let logged = error
                .source()
                .expect("the failure, which the log is given whole")
                .to_string();
            assert!(logged.contains(detail), "{logged}");

            let message = reported(error);
            assert_eq!(message, told);
            assert!(!message.contains("libsql"), "{message}");
        }
    }

    /// A cursor the browser sent is the browser's fault, not the store's, and
    /// saying so is what lets the user start over.
    #[test]
    fn a_bad_page_cursor_is_reported_as_such() {
        assert_eq!(
            reported(ApiError::from(StoreError::Cursor(CursorError::Invalid))),
            "invalid page cursor"
        );
    }

    /// The audit view lists what is running beside what has run: a batch the
    /// workflow has listed as running comes back with the finished ones, so a
    /// tab that lost a batch, or never followed it, can find it there.
    #[tokio::test]
    async fn recent_batches_list_the_running_ones_beside_the_finished_ones() {
        let backend = backend().await;
        let running = RunningBatch {
            retried_from: Some(BATCH.to_owned()),
            ..listed(INSTALLATION_ID, OTHER_BATCH)
        };
        let finished = recorded(INSTALLATION_ID, BATCH);
        backend.store().start_batch(&running).await.unwrap();
        backend.store().record_batch(&finished).await.unwrap();

        let batches = load_recent_batches_in(backend.state(), 20).await.unwrap();

        assert_eq!(
            batches,
            BatchList {
                running: vec![running],
                finished: vec![finished],
            }
        );
    }

    /// A dashboard handed a batch id alone asks the projection first, and
    /// gets the finished record whole, the running listing, or word that the
    /// projection has never heard of the id — each from the store, not
    /// Restate, which is not asked. An id the dashboard could not have minted
    /// is refused, as the progress read refuses it.
    #[tokio::test]
    async fn one_batch_is_read_from_the_projection_as_finished_running_or_unknown() {
        let backend = backend().await;
        let running = RunningBatch {
            retried_from: Some(BATCH.to_owned()),
            ..listed(INSTALLATION_ID, OTHER_BATCH)
        };
        let finished = recorded(INSTALLATION_ID, BATCH);
        backend.store().start_batch(&running).await.unwrap();
        backend.store().record_batch(&finished).await.unwrap();
        let read = |batch_id: &'static str| load_batch_projection_in(backend.state(), batch_id);

        assert_eq!(
            read(BATCH).await.unwrap(),
            Some(ProjectedBatch::Finished(finished))
        );
        assert_eq!(
            read(OTHER_BATCH).await.unwrap(),
            Some(ProjectedBatch::Running(running))
        );
        assert_eq!(read(UNKNOWN_BATCH).await.unwrap(), None);
        assert!(
            backend.forwards().is_empty(),
            "the projection answers; Restate is not asked"
        );

        assert_eq!(
            refusal(read("batch-1").await),
            InvalidBatch::BatchId.to_string()
        );
    }

    /// Where a batch stands is Restate's word, and Restate has two ways of
    /// not giving it: a 404 for a workflow it never had — a stale or foreign
    /// link — which is the `None` the follow gives up on after its thirty
    /// polls, and no answer at all, which is Restate being unavailable, and
    /// is waited out. The two must not be confused: the first read as the
    /// second polls a dead link for ever, the second read as the first gives
    /// up on a batch that is running.
    #[tokio::test]
    async fn progress_of_a_batch_restate_never_had_is_none_and_of_one_it_cannot_answer_for_is_a_fault()
     {
        let mut backend = backend().await;
        let progress = format!("BulkAction/{UNKNOWN_BATCH}/progress");
        backend.restate_refuses(&progress, StatusCode::NOT_FOUND);

        let unknown = load_batch_progress_in(backend.state(), UNKNOWN_BATCH).await;
        assert_eq!(unknown.unwrap(), None);
        backend.the_one_forward(&format!("/restate/call/{progress}"));

        backend.restate_goes_away();
        let unanswered = load_batch_progress_in(backend.state(), UNKNOWN_BATCH).await;
        assert_eq!(refusal(unanswered), "Restate is unavailable");
    }

    /// The body of the one request Restate was sent, which went to `path`.
    fn the_one_request(backend: &Backend, path: &str) -> serde_json::Value {
        serde_json::from_slice(&backend.the_one_forward(path).body).unwrap()
    }

    /// The dashboard says who is signed in by asking the server, which
    /// answers with the identity the auth edge validated. Credentials the
    /// edge no longer accepts — the password was rotated under an open tab —
    /// are refused before any server function runs, with the same challenge
    /// the page gets, and that is the 401 the browser turns into "sign in
    /// again".
    #[tokio::test]
    async fn the_signed_in_user_is_the_one_the_auth_edge_validated() {
        let dashboard = dashboard().await;

        let signed_in = dashboard
            .call("load_signed_in_user", json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(signed_in.status(), StatusCode::OK);
        assert_eq!(
            signed_in.json::<UserId>().await.unwrap(),
            UserId::new(USERNAME)
        );

        let stale = dashboard
            .call_as("load_signed_in_user", "rotated-away", json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            stale
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Basic realm=\"dependaboard\"")
        );
    }

    /// A batch target as the browser submits it.
    fn submitted(repository_id: u64, number: u64, expected_sha: &str) -> SubmittedTarget {
        SubmittedTarget {
            repository_id,
            number,
            expected_sha: expected_sha.to_owned(),
        }
    }

    /// Submits a merge batch of `targets` under `batch_id`, retrying no
    /// batch, as [`USERNAME`].
    async fn submit(
        backend: &Backend,
        batch_id: &str,
        targets: Vec<SubmittedTarget>,
    ) -> Result<BatchReceipt, ApiError> {
        submit_retrying(backend, batch_id, targets, None).await
    }

    /// Submits a merge batch of `targets` under `batch_id`, as a retry of
    /// `retried_from` when one is named, as [`USERNAME`].
    async fn submit_retrying(
        backend: &Backend,
        batch_id: &str,
        targets: Vec<SubmittedTarget>,
        retried_from: Option<&str>,
    ) -> Result<BatchReceipt, ApiError> {
        submit_batch_in(
            backend.state(),
            UserId::new(USERNAME),
            batch_id,
            BulkActionKind::Merge,
            targets,
            retried_from.map(str::to_owned),
        )
        .await
    }

    /// Posts a merge batch of `targets` under `batch_id` over HTTP, as the
    /// browser does: `targets` is the JSON it sends, whatever shape that is.
    async fn post_batch(
        dashboard: &Dashboard,
        batch_id: &str,
        targets: serde_json::Value,
        retried_from: Option<&str>,
    ) -> reqwest::Response {
        dashboard
            .call(
                "submit_batch",
                json!({
                    "batch_id": batch_id,
                    "action": "merge",
                    "targets": targets,
                    "retried_from": retried_from,
                }),
            )
            .send()
            .await
            .unwrap()
    }

    /// A batch names its targets by key and the head the user saw, and the
    /// batch it retries when it is a retry; the rest of what the workflow and
    /// the audit record say about a target is the projection's word, whatever
    /// the browser sent along. Over the wire, since what the browser sends
    /// along is the point: a target carrying fields the server does not know
    /// is decoded to the ones it does.
    #[tokio::test]
    async fn a_batch_carries_the_projections_word_on_its_targets_and_the_browsers_on_the_head_and_the_batch_retried()
     {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        let batch_id = new_batch_id();
        let mut forged = json!(submitted(7, 9, "before-the-push"));
        forged["title"] = json!("Click here");
        forged["html_url"] = json!("https://evil.example/");

        let response = post_batch(&dashboard, &batch_id, json!([forged]), Some(BATCH)).await;

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
                "retried_from": BATCH,
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

    /// A pull request the projection no longer has — merged by hand, or
    /// superseded, between the selection and the click — is one target's
    /// business, not the batch's: it is left out, named by key in the answer
    /// so the browser can account for it, and the batch runs over the rest.
    #[tokio::test]
    async fn a_target_gone_from_the_projection_is_left_out_and_the_rest_run() {
        let backend = backend().await;
        backend.project(INSTALLATION_ID, &grouped_row()).await;
        let batch_id = new_batch_id();

        let receipt = submit(
            &backend,
            &batch_id,
            vec![submitted(7, 9, "abc123"), submitted(7, 10, "abc124")],
        )
        .await
        .unwrap();

        assert_eq!(
            receipt,
            BatchReceipt {
                left_out: vec![PrKey::new(7, 10)],
            }
        );
        let request = the_one_request(
            &backend,
            &format!("/restate/send/BulkAction/{batch_id}/run"),
        );
        assert_eq!(
            request["targets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|target| target["number"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [9],
            "the batch carries only the targets the projection vouched for"
        );
    }

    /// A submission none of whose targets the projection has is no batch to
    /// run: it is refused as a whole, before Restate hears of it.
    #[tokio::test]
    async fn a_submission_with_no_resolvable_target_is_refused_whole() {
        let backend = backend().await;
        backend.project(INSTALLATION_ID, &grouped_row()).await;

        let refused = submit(
            &backend,
            &new_batch_id(),
            vec![submitted(7, 10, "abc124"), submitted(7, 11, "abc125")],
        )
        .await;

        assert_eq!(
            refusal(refused),
            "none of the 2 pull requests submitted are still in the dashboard"
        );
        assert!(backend.forwards().is_empty());
    }

    /// A pull request from another installation is not a race the dashboard
    /// can lose: the projection never showed it, so a submission naming one
    /// comes from a client that should not exist. It refuses the whole batch,
    /// and nothing reaches Restate. Over the wire, as the one test of the
    /// error envelope: a refusal out of a body reaches the browser as a 500
    /// whose `ServerError.message` is the refusal.
    #[tokio::test]
    async fn a_batch_with_a_target_from_another_installation_is_refused_whole() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        dashboard.project(INSTALLATION_ID + 1, &serde_row()).await;

        let response = post_batch(
            &dashboard,
            &new_batch_id(),
            json!([submitted(7, 9, "abc123"), submitted(8, 12, "def456")]),
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error_message(response).await,
            "pull request #12 does not belong to the configured installation"
        );
        assert!(dashboard.forwards().is_empty());
    }

    /// The batch rules come first: a submission that could not run as a
    /// batch is refused as such, before a single target is looked up, so an
    /// unknown target in it goes unmentioned.
    #[tokio::test]
    async fn a_submission_is_held_to_the_batch_rules_before_its_targets_are_resolved() {
        let backend = backend().await;
        backend.project(INSTALLATION_ID, &grouped_row()).await;
        let own = submitted(7, 9, "abc123");
        let unknown = submitted(7, 10, "abc124");

        let bad_id = submit(
            &backend,
            "550e8400-e29b-41d4-a716-446655440000",
            vec![unknown.clone()],
        )
        .await;
        assert_eq!(refusal(bad_id), "batch id must be a UUIDv7");

        let empty = submit(&backend, &new_batch_id(), vec![]).await;
        assert_eq!(
            refusal(empty),
            "batch must contain between 1 and 100 targets"
        );

        let repeated = submit(
            &backend,
            &new_batch_id(),
            vec![own.clone(), unknown, own.clone()],
        )
        .await;
        assert_eq!(refusal(repeated), "batch contains duplicate pull requests");

        let retry_of_nothing =
            submit_retrying(&backend, &new_batch_id(), vec![own], Some("batch-a")).await;
        assert_eq!(
            refusal(retry_of_nothing),
            "the batch retried must be named by a UUIDv7"
        );

        assert!(backend.forwards().is_empty());
    }

    /// The browser names a pull request by key; the projection says which
    /// repository it is in and whose it is. One the dashboard does not know,
    /// or one from another installation, is refused before Restate hears of
    /// it, and the sync request that does go out names the repository as the
    /// projection has it.
    #[tokio::test]
    async fn a_pull_request_sync_is_resolved_from_the_projection_and_held_to_the_installation() {
        let backend = backend().await;
        backend.project(INSTALLATION_ID, &grouped_row()).await;
        backend.project(INSTALLATION_ID + 1, &serde_row()).await;
        let sync = |row: PrRecord| request_pr_sync_in(backend.state(), row.key());

        let unknown = sync(PrRecord {
            number: 99,
            ..grouped_row()
        })
        .await;
        assert_eq!(
            refusal(unknown),
            "pull request #99 is no longer in the dashboard"
        );

        let foreign = sync(serde_row()).await;
        assert_eq!(
            refusal(foreign),
            "pull request #12 does not belong to the configured installation"
        );
        assert!(backend.forwards().is_empty());

        let completion_id = sync(grouped_row()).await.unwrap();
        let request = the_one_request(&backend, "/restate/send/DashboardIngress/sync_pull_request");
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

    /// Every key the browser names — for the drawer's row, its durable
    /// state, a sync, a batch target — is resolved through [`projected_pr`],
    /// which holds the key to the installation in this one place; the
    /// drawer's read of the row is that resolution and nothing else, so it
    /// is where the resolution is tested. A row of another installation is
    /// refused, not shown; a key the projection has no row for is answered
    /// with nothing, which is how the drawer learns a pull request is no
    /// longer open; and a key of this installation is answered with its row.
    #[tokio::test]
    async fn a_pull_request_row_is_read_only_within_the_installation() {
        let backend = backend().await;
        backend.project(INSTALLATION_ID, &grouped_row()).await;
        backend.project(INSTALLATION_ID + 1, &serde_row()).await;
        let read = |row: PrRecord| load_pr_projection_in(backend.state(), row.key());

        let foreign = read(serde_row()).await;
        assert_eq!(
            refusal(foreign),
            "pull request #12 does not belong to the configured installation"
        );

        let unknown = read(PrRecord {
            number: 99,
            ..grouped_row()
        })
        .await;
        assert_eq!(unknown.unwrap(), None);

        let own = read(grouped_row()).await;
        assert_eq!(
            own.unwrap(),
            Some(PrRecord {
                installation_id: INSTALLATION_ID,
                ..grouped_row()
            })
        );
    }

    /// The durable state lives in Restate, whose `PullRequest` objects know
    /// nothing of installations: whatever key is asked for is answered. So
    /// the key is held to the projection first, as every key is
    /// ([`a_pull_request_row_is_read_only_within_the_installation`]): a key
    /// with no row is answered with no state without asking Restate, the
    /// answer the drawer reads as "no longer open", and a key of this
    /// installation is answered with what Restate holds. Over the wire, as
    /// the one test that Restate is asked at the path the object key
    /// encodes to.
    #[tokio::test]
    async fn a_pull_request_status_is_asked_of_restate_only_for_a_key_the_projection_has() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        let mut synced = PrState::default();
        synced.complete_sync("sync-1".to_owned());
        for (repository_id, number) in [(7, 9), (7, 99)] {
            dashboard.restate_answers(&pr_status_path(repository_id, number), json!(synced));
        }
        let read = |row: PrRecord| {
            dashboard
                .call(
                    "load_pr_status",
                    json!({ "repository_id": row.repository_id, "number": row.number }),
                )
                .send()
        };

        let unknown = read(PrRecord {
            number: 99,
            ..grouped_row()
        })
        .await
        .unwrap();
        assert_eq!(unknown.status(), StatusCode::OK);
        assert_eq!(unknown.json::<Option<PrState>>().await.unwrap(), None);
        assert!(
            dashboard.forwards().is_empty(),
            "a key the projection does not vouch for is not asked of Restate"
        );

        let own = read(grouped_row()).await.unwrap();
        assert_eq!(own.status(), StatusCode::OK);
        assert_eq!(own.json::<Option<PrState>>().await.unwrap(), Some(synced));
        dashboard.the_one_forward("/restate/call/PullRequest/7%239/status");
    }
}
