//! The dashboard page: filters, the pull request table, selection, and the
//! bulk-action flow from confirmation through progress.

use std::collections::BTreeSet;

use dependaboard_core::{
    BatchProgress, BulkActionKind, CheckStatus, Page, PrFilter, PrRecord, PrTarget, UpdateType,
    new_batch_id,
};
use dioxus::prelude::*;

use crate::api::{load_batch_progress, load_dashboard, request_sync, submit_batch};
use crate::components::button::{Button, ButtonSize};
use crate::components::loading::{Loading, LoadingSize};
use crate::components::toast::{ToastOptions, use_toast};
use crate::ui::confirm_modal::ConfirmModal;
use crate::ui::detail_drawer::DetailDrawer;
use crate::ui::filters::{
    ActiveFilters, FacetButton, FilterSection, LabelFacets, filter_count, toggle_value,
};
use crate::ui::format::{relative_time, status_class, status_label, update_class};
use crate::ui::pr_row::PrRow;
use crate::ui::progress_drawer::ProgressDrawer;
use crate::ui::{PendingAction, pr_target, wait_one_second};

#[component]
pub(crate) fn Dashboard(mut dark: Signal<bool>) -> Element {
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

fn selected_targets(rows: &[PrRecord], selected: &BTreeSet<String>) -> Vec<PrTarget> {
    rows.iter()
        .filter(|row| selected.contains(&row.id))
        .map(pr_target)
        .collect()
}
