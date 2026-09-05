//! The bar that appears over a selection, offering the bulk actions for it.

use dependaboard_core::BulkActionKind;
use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::ui::PendingAction;
use crate::ui::dashboard_state::use_dashboard;

/// The action bar; nothing while nothing is selected. `onrequest` receives
/// the action the user asked for, with the selection resolved to targets at
/// that moment.
#[component]
pub(crate) fn ActionBar(onrequest: EventHandler<PendingAction>) -> Element {
    let mut state = use_dashboard();
    let selected_count = state.selected_count();
    if selected_count == 0 {
        return rsx! {};
    }
    let request = move |action: BulkActionKind| {
        onrequest.call(PendingAction {
            action,
            targets: state.selected_targets(),
        });
    };
    rsx! {
        div { class: "action-bar",
            strong { class: "mono", "{selected_count} selected" }
            button { class: "action-link", onclick: move |_| state.clear_selection(), "clear" }
            span { class: "action-divider" }
            Button {
                size: ButtonSize::Sm,
                class: "rebase-button",
                onclick: move |_| request(BulkActionKind::Rebase),
                "Request rebase"
            }
            Button {
                size: ButtonSize::Sm,
                class: "update-branch-button",
                onclick: move |_| request(BulkActionKind::UpdateBranch),
                "Update branch"
            }
            Button {
                size: ButtonSize::Sm,
                class: "merge-button",
                onclick: move |_| request(BulkActionKind::Merge),
                "Merge selected"
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::ui::dashboard_state::PageStatus;
    use crate::ui::test_support::{DashboardFixture, loaded_page, render};

    #[test]
    fn the_bar_stays_away_until_something_is_selected() {
        fn Fixture() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    ActionBar { onrequest: move |_| {} }
                }
            }
        }
        let html = render(Fixture);

        assert!(!html.contains("action-bar"), "{html}");
    }

    #[test]
    fn the_bar_counts_the_selection_and_offers_every_action() {
        fn Fixture() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected: BTreeSet::from(["7#9".to_owned(), "8#12".to_owned()]),
                    ActionBar { onrequest: move |_| {} }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains("2 selected"), "{html}");
        assert!(html.contains("Request rebase"), "{html}");
        assert!(html.contains("Update branch"), "{html}");
        assert!(html.contains("Merge selected"), "{html}");
    }
}
