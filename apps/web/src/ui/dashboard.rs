//! The dashboard page: filters, the pull request table, selection, and the
//! bulk-action flow from confirmation through progress.

use std::collections::BTreeSet;

use dependaboard_core::{
    BatchProgress, BulkActionKind, CheckStatus, DEFAULT_PAGE_SIZE, Page, PrFilter, PrRecord,
    PrTarget, UpdateType, new_batch_id,
};
use dioxus::prelude::*;

use crate::api::{load_dashboard, request_sync};
use crate::components::button::{Button, ButtonSize};
use crate::components::loading::{Loading, LoadingSize};
use crate::components::toast::{ToastOptions, use_toast};
use crate::ui::batch::{BatchOutcome, STALL_TIMEOUT, SUBMIT_ATTEMPTS, ServerBatch, run_batch};
use crate::ui::confirm_modal::ConfirmModal;
use crate::ui::detail_drawer::DetailDrawer;
use crate::ui::filters::{
    ActiveFilters, FacetButton, FilterSection, LabelFacets, filter_count, toggle_value,
};
use crate::ui::format::{pull_requests, relative_time, status_class, status_label, update_class};
use crate::ui::pr_row::PrRow;
use crate::ui::progress_drawer::ProgressDrawer;
use crate::ui::search_box::SearchBox;
use crate::ui::{POLL_INTERVAL, PendingAction, pr_target, sleep, user_facing};

#[component]
pub(crate) fn Dashboard(mut dark: Signal<bool>) -> Element {
    let toast = use_toast();
    let mut aside_open = use_signal(|| true);
    let mut filter = use_signal(PrFilter::default);
    let mut cursor = use_signal(|| None::<String>);
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
            after: cursor(),
            ..Page::default()
        };
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
        .map(user_facing);
    let rows = page
        .as_ref()
        .map(|page| page.rows.clone())
        .unwrap_or_default();
    let total = page.as_ref().map(|page| page.total).unwrap_or_default();
    let selected_count = selected.read().len();
    let active_filter_count = filter_count(&filter());

    let mut reload = move || dashboard.restart();

    let queue_batch = move |PendingAction { action, targets }: PendingAction| {
        let batch_id = new_batch_id();
        active_batch.set(Some(BatchProgress::queued(&batch_id, action, &targets)));
        progress_open.set(true);
        selected.write().clear();
        spawn(async move {
            let mut batch = ServerBatch {
                batch_id,
                action,
                targets,
            };
            let outcome = run_batch(&mut batch, |progress| {
                active_batch.set(Some(progress.clone()));
            })
            .await;
            match outcome {
                BatchOutcome::Completed(progress) => {
                    match progress.failure {
                        Some(failure) => toast.error(format!("Batch failed: {failure}"), sticky()),
                        None => toast.success("Batch complete".to_owned(), ToastOptions::new()),
                    }
                    reload();
                }
                BatchOutcome::NotSubmitted(error) => toast.error(
                    format!("Batch was not submitted after {SUBMIT_ATTEMPTS} attempts: {error}"),
                    sticky(),
                ),
                BatchOutcome::Stalled(error) => {
                    let reason = error.unwrap_or_else(|| {
                        format!("no progress for {} minutes", STALL_TIMEOUT.as_secs() / 60)
                    });
                    toast.error(
                        format!(
                            "Lost track of the batch ({reason}). It may still be running in Restate; reload to see where it got to."
                        ),
                        sticky(),
                    );
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
                            toast.error(format!("Sync failed: {}", user_facing(&error)), sticky());
                        } else {
                            toast.info("Reconciliation queued".to_owned(), ToastOptions::new());
                            sleep(POLL_INTERVAL).await;
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
                SearchBox { filter, cursor }
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
                    span { class: "mono", "{pull_requests(total)}" }
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
                        EmptyState { filtered: active_filter_count > 0 }
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
                                "Load next {DEFAULT_PAGE_SIZE}"
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
            repositories: page.as_ref().map(|page| page.repositories.clone()).unwrap_or_default(),
            oncancel: move |_| pending.set(None),
            onconfirm: queue_batch,
        }
    }
}

/// Errors stay until dismissed; every other toast auto-dismisses.
fn sticky() -> ToastOptions {
    ToastOptions::new().permanent(true)
}

/// The table body when the page has no rows. `filtered` says whether filters
/// are active, because "nothing matches" and "nothing is open" call for
/// different next steps.
#[component]
fn EmptyState(filtered: bool) -> Element {
    rsx! {
        div { class: "empty-state",
            span { class: "empty-mark" }
            if filtered {
                strong { "No pull requests match these filters" }
                p { "Clear a filter to widen the list." }
            } else {
                strong { "No open Dependabot pull requests" }
                p { "Queue a reconciliation sweep to check GitHub again." }
            }
        }
    }
}

fn selected_targets(rows: &[PrRecord], selected: &BTreeSet<String>) -> Vec<PrTarget> {
    rows.iter()
        .filter(|row| selected.contains(&row.id))
        .map(pr_target)
        .collect()
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

    fn render_empty_state(filtered: bool) -> String {
        let mut dom = VirtualDom::new_with_props(EmptyState, EmptyStateProps { filtered });
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    #[test]
    fn an_empty_page_says_whether_filters_hid_the_pull_requests() {
        let filtered = render_empty_state(true);
        assert!(filtered.contains("No pull requests match"), "{filtered}");
        assert!(!filtered.contains("No open Dependabot"), "{filtered}");

        let unfiltered = render_empty_state(false);
        assert!(
            unfiltered.contains("No open Dependabot pull requests"),
            "{unfiltered}"
        );
        assert!(!unfiltered.contains("filter"), "{unfiltered}");
    }
}
