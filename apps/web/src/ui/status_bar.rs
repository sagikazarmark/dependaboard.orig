//! The status bar along the bottom: whether the projection is online and
//! when it last heard from GitHub.

use dioxus::prelude::*;

use crate::ui::dashboard_state::use_dashboard;
use crate::ui::format::relative_time;

#[component]
pub(crate) fn StatusBar() -> Element {
    let state = use_dashboard();
    let status = state.page.read();
    let last_synced_at = status.loaded().and_then(|page| page.last_synced_at);
    rsx! {
        footer { class: "statusbar",
            span { class: "status-dot" }
            span { "projection online" }
            span { class: "status-spacer" }
            if let Some(synced) = last_synced_at {
                span { "last event {relative_time(synced)}" }
            } else {
                span { "waiting for first reconciliation" }
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::ui::dashboard_state::PageStatus;
    use crate::ui::test_support::{DashboardFixture, loaded_page, render};

    #[test]
    fn the_status_bar_dates_the_last_event_once_there_has_been_one() {
        fn Loading() -> Element {
            rsx! {
                DashboardFixture { StatusBar {} }
            }
        }
        let loading = render(Loading);
        assert!(
            loading.contains("waiting for first reconciliation"),
            "{loading}"
        );

        fn Loaded() -> Element {
            rsx! {
                DashboardFixture { page: PageStatus::Loaded(loaded_page()), StatusBar {} }
            }
        }
        let loaded = render(Loaded);
        assert!(loaded.contains("last event now"), "{loaded}");
        assert!(!loaded.contains("waiting for"), "{loaded}");
    }
}
