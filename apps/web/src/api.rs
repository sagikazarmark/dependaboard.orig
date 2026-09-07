//! The server functions the dashboard calls. Compiled on both targets: the
//! wasm build gets the client stubs, the server build the bodies, which reach
//! the store and Restate through [`crate::server::state::ServerState`].

use dependaboard_core::{
    BatchList, BatchProgress, BatchReceipt, BulkActionKind, Capabilities, DashboardPage,
    DashboardSummary, Page, PrFilter, PrRecord, PrState, ProjectionRevision, SubmittedTarget,
    UserId,
};
use dioxus::prelude::*;

#[cfg(feature = "server")]
use {
    crate::server::{restate::pr_status_path, state::ServerState},
    axum::extract::Extension,
    dependaboard_core::{
        BulkRequest, InvalidBatch, ManualSyncRequest, PrKey, PrTarget, new_batch_id, validate_batch,
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

/// Who the auth edge let this request through as. The browser holds the
/// credentials but cannot read them, so this is how the dashboard learns
/// whose name to show; the answer is the identity a batch will be recorded
/// under.
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
    state
        .ingress
        .call("DashboardIngress/capabilities")
        .await
        .map_err(restate_unavailable)
}

/// Asks Restate to run the batch. The batch id is the workflow key, which lets
/// a workflow run once, and the idempotency key, which lets the browser resend
/// the same submission after a lost response and be told it was accepted
/// rather than refused for the workflow already existing.
///
/// The browser names each target by key and the head it saw; the rest of the
/// target is resolved here from the projection. A target the projection no
/// longer has — merged or closed between the selection and the click — is one
/// pull request's business, not the batch's: it is left out, named by key in
/// the receipt, and the batch runs over the rest. A target from another
/// installation is no race but a client that should not exist, and refuses
/// the batch whole; so does a submission that leaves nothing to run. The
/// submission is held to the batch rules first, so one that could not run
/// resolves nothing.
#[server(state: Extension<ServerState>, user: Extension<UserId>)]
pub(crate) async fn submit_batch(
    batch_id: String,
    action: BulkActionKind,
    targets: Vec<SubmittedTarget>,
) -> Result<BatchReceipt, ServerFnError> {
    validate_batch(&batch_id, targets.iter().map(SubmittedTarget::key))
        .map_err(|invalid| ServerFnError::new(invalid.to_string()))?;
    let submitted = targets.len();
    let mut resolved = Vec::with_capacity(submitted);
    let mut left_out = Vec::new();
    for target in targets {
        let key = target.key();
        let Some(row) = projected_pr(&state, &key).await? else {
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
        return Err(ServerFnError::new(format!(
            "none of the {submitted} pull requests submitted are still in the dashboard"
        )));
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
        .map_err(restate_unavailable)?;
    Ok(BatchReceipt { left_out })
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

/// The batches the audit view lists: every batch running, and the `limit`
/// most recently finished, newest first, as the projection keeps them: what
/// was asked, by whom, when, and — once finished — how each target went. Read
/// from the store, not Restate, so a finished batch is still here after the
/// workflow's retention has cleared its progress, and a running one is found
/// without knowing its id. `limit` is held to
/// [`MAX_RECENT_BATCHES`](dependaboard_core::MAX_RECENT_BATCHES).
#[server(state: Extension<ServerState>)]
pub(crate) async fn load_recent_batches(limit: u32) -> Result<BatchList, ServerFnError> {
    let running = state.store.running_batches().await.map_err(store_failure)?;
    let finished = state
        .store
        .recent_batches(limit.clamp(1, dependaboard_core::MAX_RECENT_BATCHES))
        .await
        .map_err(store_failure)?;
    Ok(BatchList { running, finished })
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
    let key = PrKey::new(repository_id, number);
    let row = projected_pr(&state, &key)
        .await?
        .ok_or_else(|| no_longer_in_the_dashboard(&key))?;
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

/// The pull request the browser named, as the projection has it, or `None`
/// for one the projection does not: the browser only ever names a key, and
/// the projection's row is the word on which installation the pull request
/// is in and what it is. A key whose repository belongs to another
/// installation is refused outright rather than answered with nothing: the
/// projection never showed it, so no dashboard could have named it.
#[cfg(feature = "server")]
async fn projected_pr(state: &ServerState, key: &PrKey) -> Result<Option<PrRecord>, ServerFnError> {
    let Some(row) = state.store.get_pr(key).await.map_err(store_failure)? else {
        return Ok(None);
    };
    if row.installation_id != state.installation_id {
        return Err(ServerFnError::new(format!(
            "pull request #{} does not belong to the configured installation",
            key.number
        )));
    }
    Ok(Some(row))
}

/// Refuses a request for a pull request the projection no longer has, where
/// the request has nothing to do without it.
#[cfg(feature = "server")]
fn no_longer_in_the_dashboard(key: &PrKey) -> ServerFnError {
    ServerFnError::new(format!(
        "pull request #{} is no longer in the dashboard",
        key.number
    ))
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
    use dependaboard_core::{
        BatchRecord, BatchTargetRecord, CursorError, RunningBatch, TargetOutcome,
    };
    use reqwest::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::server::test_support::{
        Dashboard, INSTALLATION_ID, USERNAME, dashboard, error_message,
    };
    use crate::ui::test_support::{BATCH, GROUPED_ROW_TITLE, OTHER_BATCH, grouped_row, serde_row};

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

    /// The audit view lists what is running beside what has run: a batch the
    /// workflow has listed as running comes back with the finished ones, so a
    /// tab that lost a batch, or never followed it, can find it there.
    #[tokio::test]
    async fn recent_batches_list_the_running_ones_beside_the_finished_ones() {
        let dashboard = dashboard().await;
        let running = RunningBatch {
            batch_id: OTHER_BATCH.to_owned(),
            action: BulkActionKind::Merge,
            requested_by: UserId::new(USERNAME),
            started_at: 2_000,
            target_count: 3,
        };
        let finished = BatchRecord {
            batch_id: BATCH.to_owned(),
            action: BulkActionKind::Rebase,
            requested_by: UserId::new(USERNAME),
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
                outcome: TargetOutcome::Succeeded {
                    detail: "@dependabot rebase posted".to_owned(),
                },
            }],
        };
        dashboard.store().start_batch(&running).await.unwrap();
        dashboard.store().record_batch(&finished).await.unwrap();

        let response = dashboard
            .call("load_recent_batches", json!({ "limit": 20 }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<BatchList>().await.unwrap(),
            BatchList {
                running: vec![running],
                finished: vec![finished],
            }
        );
    }

    /// The body of the one request Restate was sent, which went to `path`.
    fn the_one_request(dashboard: &Dashboard, path: &str) -> serde_json::Value {
        serde_json::from_slice(&dashboard.the_one_forward(path).body).unwrap()
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

    /// A pull request the projection no longer has — merged by hand, or
    /// superseded, between the selection and the click — is one target's
    /// business, not the batch's: it is left out, named by key in the answer
    /// so the browser can account for it, and the batch runs over the rest.
    #[tokio::test]
    async fn a_target_gone_from_the_projection_is_left_out_and_the_rest_run() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        let batch_id = new_batch_id();

        let response = submit(
            &dashboard,
            &batch_id,
            json!([target(7, 9, "abc123"), target(7, 10, "abc124")]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<BatchReceipt>().await.unwrap(),
            BatchReceipt {
                left_out: vec![PrKey::new(7, 10)],
            }
        );
        let request = the_one_request(
            &dashboard,
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
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;

        let response = submit(
            &dashboard,
            &new_batch_id(),
            json!([target(7, 10, "abc124"), target(7, 11, "abc125")]),
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            error_message(response).await,
            "none of the 2 pull requests submitted are still in the dashboard"
        );
        assert!(dashboard.forwards().is_empty());
    }

    /// A pull request from another installation is not a race the dashboard
    /// can lose: the projection never showed it, so a submission naming one
    /// comes from a client that should not exist. It refuses the whole batch,
    /// and nothing reaches Restate.
    #[tokio::test]
    async fn a_batch_with_a_target_from_another_installation_is_refused_whole() {
        let dashboard = dashboard().await;
        dashboard.project(INSTALLATION_ID, &grouped_row()).await;
        dashboard.project(INSTALLATION_ID + 1, &serde_row()).await;

        let response = submit(
            &dashboard,
            &new_batch_id(),
            json!([target(7, 9, "abc123"), target(8, 12, "def456")]),
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
