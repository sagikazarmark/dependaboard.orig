//! The dashboard page: filters, the pull request table, selection, and the
//! bulk-action flow from confirmation through progress. The filter, cursor,
//! and open pull request live in the URL as well, so a view can be shared and
//! survives a refresh. The rows keep themselves current: the page follows the
//! read model's revision and reloads when it moves — and says so, over the
//! page, when the polls that follow it stop being answered.

use dependaboard_core::{Page, PrRecord, unix_seconds};
use dioxus::core::Task;
use dioxus::logger::tracing;
use dioxus::prelude::*;

use crate::api::{load_capabilities, load_dashboard, load_signed_in_user, load_summary};
use crate::components::toast::{ToastOptions, use_toast};
use crate::ui::action_bar::ActionBar;
use crate::ui::active_batch::{ActiveBatch, BatchHost, attach_batch, queue_batch};
use crate::ui::batch::Followed;
use crate::ui::confirm_modal::ConfirmModal;
use crate::ui::dashboard_state::{
    Answers, CapabilitiesStatus, DashboardState, PageStatus, Selection, SummaryStatus,
};
use crate::ui::detail_drawer::{OpenDetail, OpenPr};
use crate::ui::live::{use_clock, use_live_refresh, use_visibility};
use crate::ui::pr_table::PrTable;
use crate::ui::recent_batches::RecentBatchesDrawer;
use crate::ui::sidebar::Sidebar;
use crate::ui::status_bar::{ConnectionBanner, StatusBar};
use crate::ui::top_bar::TopBar;
use crate::ui::url_state::UrlState;
use crate::ui::url_sync::{use_url_state, use_url_sync};
use crate::ui::{PendingAction, sticky};

/// The page. It owns the state the components share, loads the read model
/// for the filter and cursor in force, and holds what is open over the page:
/// the pull request drawer, the bulk action awaiting confirmation, the batch
/// being followed, and the list of the batches that have run. It opens on the
/// state its URL names and keeps the URL in step from then on.
///
/// The rows and the facets are two requests: the rows follow the filter and
/// the cursor, the facets the filter alone, so loading the next page leaves
/// the facets as they are. Both are asked again when the read model's
/// revision moves, which the page polls for while the tab is showing.
#[component]
pub(crate) fn Dashboard(dark: Signal<bool>) -> Element {
    let toast = use_toast();
    let aside_open = use_signal(|| true);
    let mut batches_open = use_signal(|| false);
    let UrlState {
        filter: initial_filter,
        cursor: initial_cursor,
        pr: initial_pr,
        ..
    } = use_url_state();
    let filter = use_signal(|| initial_filter);
    let cursor = use_signal(|| initial_cursor);
    let selected = use_signal(Selection::default);
    let mut detail = use_signal(|| initial_pr.map(OpenPr::Loading));
    let mut pending = use_signal(|| None::<PendingAction>);
    let followed = use_signal(|| None::<Followed>);
    let follower = use_signal(|| None::<Task>);
    let progress_open = use_signal(|| false);
    let visible = use_visibility();
    // The clock the relative times are read against; driven once the state
    // is up, since it stops with the polls when the page is signed out.
    let now = use_signal(unix_seconds);

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
    // Who the server let the page in as. Asked once, since the identity does
    // not change under an open page; a page the server has stopped letting
    // in is told so by the polls, not by asking this again.
    let mut signed_in = use_resource(|| async {
        load_signed_in_user().await.inspect_err(|error| {
            tracing::debug!(%error, "who is signed in could not be read");
        })
    });
    // What the service can do. Asked once too: it is settled when the service
    // starts, and a running service does not change its mind.
    let mut capabilities = use_resource(|| async {
        load_capabilities().await.inspect_err(|error| {
            tracing::debug!(%error, "what the service can do could not be read");
        })
    });
    let capabilities_status =
        use_memo(move || CapabilitiesStatus::from_resource(capabilities.read().as_ref()));
    // The resources keep their last answer while the next is in flight, so a
    // reload leaves the rows standing until the fresh ones land. The name and
    // the capabilities are asked again only if they were never answered: a
    // reload is also what follows an outage, and an outage at open leaves
    // them unanswered.
    let reload = use_callback(move |()| {
        rows.restart();
        summary.restart();
        if signed_in.peek().as_ref().is_some_and(Result::is_err) {
            signed_in.restart();
        }
        if capabilities.peek().as_ref().is_some_and(Result::is_err) {
            capabilities.restart();
        }
    });
    let mut state = DashboardState::provide(
        filter,
        cursor,
        selected,
        Answers {
            page: page.into(),
            summary: summary_status.into(),
            capabilities: capabilities_status.into(),
        },
        now,
        reload,
    );
    let host = BatchHost {
        followed,
        follower,
        open: progress_open,
        toast,
        state,
    };
    // The batch the URL names is followed by id: after a reload, this is how
    // the drawer reopens on the batch the page was following.
    let onbatch = use_callback(move |batch_id: String| attach_batch(batch_id, host));
    use_url_sync(state, detail, followed, onbatch);
    use_clock(now, visible, state);
    use_live_refresh(state, visible);
    let user = signed_in
        .read()
        .as_ref()
        .and_then(|answer| answer.as_ref().ok())
        .map(ToString::to_string);
    let summary = summary_status.read();

    rsx! {
        TopBar { dark, aside_open, batches_open }
        ConnectionBanner {}

        div { class: "workspace",
            Sidebar { open: aside_open }
            PrTable { onopen: move |row: PrRecord| detail.set(Some(row.into())) }
        }

        StatusBar { user }

        ActionBar { onrequest: move |action| pending.set(Some(action)) }

        if batches_open() {
            RecentBatchesDrawer {
                onclose: move |_| batches_open.set(false),
                // Following a batch from the list opens the progress drawer
                // on it, which takes the list's place over the page.
                onfollow: move |batch_id| {
                    batches_open.set(false);
                    attach_batch(batch_id, host);
                },
            }
        }

        OpenDetail {
            detail,
            now: state.now(),
            onaction: move |action| pending.set(Some(action)),
            onsync: move |result: Result<Option<PrRecord>, String>| match result {
                Ok(Some(row)) => {
                    detail.set(Some(row.into()));
                    toast.success("Pull request synced".to_owned(), ToastOptions::new());
                    state.reload();
                }
                Ok(None) => {
                    detail.set(None);
                    toast.info("Pull request is no longer open".to_owned(), ToastOptions::new());
                    state.reload();
                }
                Err(error) => toast.error(error, sticky()),
            },
        }

        ActiveBatch { followed, follower, open: progress_open }

        ConfirmModal {
            pending: pending(),
            repositories: (*summary).map(|summary| {
                summary
                    .facets
                    .repositories
                    .iter()
                    .map(|facet| facet.repository.clone())
                    .collect()
            }),
            oncancel: move |_| pending.set(None),
            // The rows leave the selection once Restate has the batch, not
            // before: a submission refused whole leaves them standing to try
            // again, and the rest of the selection stands either way, so one
            // pull request merged from its drawer does not drop the twenty
            // picked for the next batch.
            onconfirm: move |action: PendingAction| queue_batch(action, host),
        }
    }
}
