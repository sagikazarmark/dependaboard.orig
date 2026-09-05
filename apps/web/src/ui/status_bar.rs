//! The status bar along the bottom: whether the projection is online and
//! when it last heard from GitHub.

use dioxus::prelude::*;

use crate::ui::dashboard_state::use_dashboard;
use crate::ui::format::relative_time;

#[component]
pub(crate) fn StatusBar() -> Element {
    let state = use_dashboard();
    let summary = state.summary.read();
    let last_synced_at = summary.loaded().and_then(|summary| summary.last_synced_at);
    let now = state.now();
    rsx! {
        footer { class: "statusbar",
            span { class: "status-dot" }
            span { "projection online" }
            span { class: "status-spacer" }
            if let Some(synced) = last_synced_at {
                span { "last event {relative_time(now, synced)}" }
            } else {
                span { "waiting for first reconciliation" }
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::ui::dashboard_state::SummaryStatus;
    use crate::ui::test_support::{DashboardFixture, FIXTURE_NOW, loaded_summary, render};

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
                DashboardFixture { summary: SummaryStatus::Loaded(loaded_summary()), StatusBar {} }
            }
        }
        let loaded = render(Loaded);
        assert!(loaded.contains("last event now"), "{loaded}");
        assert!(!loaded.contains("waiting for"), "{loaded}");
    }

    /// The date is read against the dashboard's clock, so it moves on as the
    /// clock ticks, with no new answer from the read model.
    #[test]
    fn the_last_event_ages_with_the_dashboards_clock() {
        fn Later() -> Element {
            rsx! {
                DashboardFixture {
                    summary: SummaryStatus::Loaded(loaded_summary()),
                    now: FIXTURE_NOW + 5 * 60,
                    StatusBar {}
                }
            }
        }
        let later = render(Later);

        assert!(later.contains("last event 5m"), "{later}");
    }
}
