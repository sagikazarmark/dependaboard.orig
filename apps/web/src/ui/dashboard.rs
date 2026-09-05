//! The dashboard page: filters, the pull request table, selection, and the
//! bulk-action flow from confirmation through progress.

use std::collections::BTreeSet;

use dependaboard_core::{BatchProgress, Page, PrFilter, PrRecord};
use dioxus::prelude::*;

use crate::api::load_dashboard;
use crate::components::toast::{ToastOptions, use_toast};
use crate::ui::action_bar::ActionBar;
use crate::ui::active_batch::{ActiveBatch, queue_batch};
use crate::ui::confirm_modal::ConfirmModal;
use crate::ui::dashboard_state::{DashboardState, PageStatus};
use crate::ui::detail_drawer::OpenDetail;
use crate::ui::pr_table::PrTable;
use crate::ui::sidebar::Sidebar;
use crate::ui::status_bar::StatusBar;
use crate::ui::top_bar::TopBar;
use crate::ui::{PendingAction, sticky};

/// The page. It owns the state the components share, loads the read model
/// for the filter and cursor in force, and holds what is open over the page:
/// the pull request drawer, the bulk action awaiting confirmation, and the
/// batch being followed.
#[component]
pub(crate) fn Dashboard(dark: Signal<bool>) -> Element {
    let toast = use_toast();
    let aside_open = use_signal(|| true);
    let filter = use_signal(PrFilter::default);
    let cursor = use_signal(|| None::<String>);
    let selected = use_signal(BTreeSet::<String>::new);
    let mut detail = use_signal(|| None::<PrRecord>);
    let mut pending = use_signal(|| None::<PendingAction>);
    let active_batch = use_signal(|| None::<BatchProgress>);
    let progress_open = use_signal(|| false);

    let mut dashboard = use_resource(move || {
        let filter = filter();
        let page = Page {
            after: cursor(),
            ..Page::default()
        };
        async move { load_dashboard(filter, page).await }
    });
    let page = use_memo(move || PageStatus::from_resource(dashboard.read().as_ref()));
    let reload = use_callback(move |()| dashboard.restart());
    let mut state = DashboardState::provide(filter, cursor, selected, page, reload);
    let page = page.read();

    rsx! {
        TopBar { dark, aside_open }

        div { class: "workspace",
            Sidebar { open: aside_open }
            PrTable { onopen: move |row: PrRecord| detail.set(Some(row)) }
        }

        StatusBar {}

        ActionBar { onrequest: move |action| pending.set(Some(action)) }

        OpenDetail {
            detail,
            onaction: move |action| pending.set(Some(action)),
            onsync: move |result: Result<Option<PrRecord>, String>| match result {
                Ok(Some(row)) => {
                    detail.set(Some(row));
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
            repositories: page.loaded().map(|page| page.repositories.clone()).unwrap_or_default(),
            oncancel: move |_| pending.set(None),
            onconfirm: move |action| {
                state.clear_selection();
                queue_batch(action, active_batch, progress_open, toast, reload);
            },
        }
    }
}
