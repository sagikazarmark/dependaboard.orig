//! The drawer for one pull request: its projection, durable state, and the
//! per-row rebase, merge, and sync actions.

use dependaboard_core::{BulkActionKind, PrRecord, PrState, unix_seconds};
use dioxus::prelude::*;

use crate::api::{load_pr_projection, load_pr_status, request_pr_sync};
use crate::components::button::{Button, ButtonSize};
use crate::components::loading::{Loading, LoadingSize};
use crate::ui::format::{relative_time, status_class, status_label, update_class, version_label};
use crate::ui::side_panel::SidePanel;
use crate::ui::{PendingAction, pr_target, wait_one_second};

#[component]
pub(crate) fn DetailDrawer(
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

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::Mergeable;

    use super::*;
    use crate::ui::test_support::grouped_row;

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
    fn pull_request_sync_completes_only_for_its_request_id() {
        let mut state = PrState::default();
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-456".to_owned());
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-123".to_owned());
        assert!(sync_id_completed(Some(&state), "sync-123"));
    }
}
