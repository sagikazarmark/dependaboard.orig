//! The dashboard page: filters, the pull request table, selection, and the
//! bulk-action flow from confirmation through progress. The filter, cursor,
//! and open pull request live in the URL as well, so a view can be shared and
//! survives a refresh.

use std::collections::BTreeSet;

use dependaboard_core::{BatchProgress, Page, PrRecord};
use dioxus::prelude::*;

use crate::api::{load_dashboard, load_summary};
use crate::components::toast::{ToastOptions, use_toast};
use crate::ui::action_bar::ActionBar;
use crate::ui::active_batch::{ActiveBatch, queue_batch};
use crate::ui::confirm_modal::ConfirmModal;
use crate::ui::dashboard_state::{DashboardState, PageStatus, SummaryStatus};
use crate::ui::detail_drawer::{OpenDetail, OpenPr};
use crate::ui::pr_table::PrTable;
use crate::ui::sidebar::Sidebar;
use crate::ui::status_bar::StatusBar;
use crate::ui::top_bar::TopBar;
use crate::ui::url_state::UrlState;
use crate::ui::url_sync::{use_url_state, use_url_sync};
use crate::ui::{PendingAction, sticky};

/// The page. It owns the state the components share, loads the read model
/// for the filter and cursor in force, and holds what is open over the page:
/// the pull request drawer, the bulk action awaiting confirmation, and the
/// batch being followed. It opens on the state its URL names and keeps the
/// URL in step from then on.
///
/// The rows and the facets are two requests: the rows follow the filter and
/// the cursor, the facets the filter alone, so loading the next page leaves
/// the facets as they are.
#[component]
pub(crate) fn Dashboard(dark: Signal<bool>) -> Element {
    let toast = use_toast();
    let aside_open = use_signal(|| true);
    let UrlState {
        filter: initial_filter,
        cursor: initial_cursor,
        pr: initial_pr,
    } = use_url_state();
    let filter = use_signal(|| initial_filter);
    let cursor = use_signal(|| initial_cursor);
    let selected = use_signal(BTreeSet::<String>::new);
    let mut detail = use_signal(|| initial_pr.map(OpenPr::Loading));
    let mut pending = use_signal(|| None::<PendingAction>);
    let active_batch = use_signal(|| None::<BatchProgress>);
    let progress_open = use_signal(|| false);

    let mut rows = use_resource(move || {
        let filter = filter();
        let page = Page {
            after: cursor(),
            ..Page::default()
        };
        async move { load_dashboard(filter, page).await }
    });
    let mut summary = use_resource(move || {
        let filter = filter();
        async move { load_summary(filter).await }
    });
    let page = use_memo(move || PageStatus::from_resource(rows.read().as_ref()));
    let summary_status = use_memo(move || SummaryStatus::from_resource(summary.read().as_ref()));
    let reload = use_callback(move |()| {
        rows.restart();
        summary.restart();
    });
    let mut state = DashboardState::provide(filter, cursor, selected, page, summary_status, reload);
    use_url_sync(state, detail);
    let summary = summary_status.read();

    rsx! {
        TopBar { dark, aside_open }

        div { class: "workspace",
            Sidebar { open: aside_open }
            PrTable { onopen: move |row: PrRecord| detail.set(Some(row.into())) }
        }

        StatusBar {}

        ActionBar { onrequest: move |action| pending.set(Some(action)) }

        OpenDetail {
            detail,
            onaction: move |action| pending.set(Some(action)),
            onsync: move |result: Result<Option<PrRecord>, String>| match result {
                Ok(Some(row)) => {
                    detail.set(Some(row.into()));
                    toast.success("Pull request synced".to_owned(), ToastOptions::new());
                    reload.call(());
                }
                Ok(None) => {
                    detail.set(None);
                    toast.info("Pull request is no longer open".to_owned(), ToastOptions::new());
                    reload.call(());
                }
                Err(error) => toast.error(error, sticky()),
            },
        }

        ActiveBatch { progress: active_batch, open: progress_open }

        ConfirmModal {
            pending: pending(),
            repositories: summary
                .loaded()
                .map(|summary| {
                    summary
                        .facets
                        .repositories
                        .iter()
                        .map(|facet| facet.repository.clone())
                        .collect()
                })
                .unwrap_or_default(),
            oncancel: move |_| pending.set(None),
            onconfirm: move |action| {
                state.clear_selection();
                queue_batch(action, active_batch, progress_open, toast, reload);
            },
        }
    }
}
