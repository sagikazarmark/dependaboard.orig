//! The bar that appears over a selection, offering the bulk actions for it.

use dependaboard_core::{BulkActionKind, MAX_BATCH_TARGETS};
use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::ui::PendingAction;
use crate::ui::dashboard_state::use_dashboard;
use crate::ui::format::repositories;

/// The action bar; nothing while nothing is selected. `onrequest` receives
/// the action the user asked for, with the selection resolved to rows at
/// that moment. A selection larger than one batch takes has its actions
/// withheld until it is trimmed: rows picked one by one on later pages can
/// push a selection past the limit, and the server would refuse the batch.
/// A rebase is withheld on its own, with the reason, once the service has
/// said it has no user token to post one with.
#[component]
pub(crate) fn ActionBar(onrequest: EventHandler<PendingAction>) -> Element {
    let mut state = use_dashboard();
    let selected_count = state.selected_count();
    if selected_count == 0 {
        return rsx! {};
    }
    let spanned = repositories(state.selected_repository_count());
    let over_limit = selected_count.saturating_sub(MAX_BATCH_TARGETS);
    let note = if over_limit > 0 {
        Some(format!(
            "a batch takes at most {MAX_BATCH_TARGETS} pull requests; deselect {over_limit}"
        ))
    } else {
        state.capped_from().map(|total| {
            format!(
                "the first {selected_count} of {total} matching; a batch takes at most {MAX_BATCH_TARGETS}"
            )
        })
    };
    let rebase_withheld = state.rebase_withheld();
    let request = move |action: BulkActionKind| {
        onrequest.call(PendingAction {
            action,
            rows: state.selected_rows(),
            retried_from: None,
        });
    };
    rsx! {
        div { class: "action-bar",
            strong { class: "mono", "{selected_count} selected" }
            span { class: "action-summary mono", "{spanned}" }
            button { class: "action-link", onclick: move |_| state.clear_selection(), "clear" }
            if let Some(note) = note {
                span { class: "action-note", "{note}" }
            }
            if let Some(reason) = rebase_withheld {
                span { class: "action-note", "{reason}" }
            }
            span { class: "action-divider" }
            Button {
                size: ButtonSize::Sm,
                class: "rebase-button",
                disabled: over_limit > 0 || rebase_withheld.is_some(),
                onclick: move |_| request(BulkActionKind::Rebase),
                "Request rebase"
            }
            Button {
                size: ButtonSize::Sm,
                class: "update-branch-button",
                disabled: over_limit > 0,
                onclick: move |_| request(BulkActionKind::UpdateBranch),
                "Update branch"
            }
            Button {
                size: ButtonSize::Sm,
                class: "merge-button",
                disabled: over_limit > 0,
                onclick: move |_| request(BulkActionKind::Merge),
                "Merge selected"
            }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{Capabilities, PrRecord};

    use super::*;
    use crate::ui::dashboard_state::{CapabilitiesStatus, PageStatus, REBASE_UNAVAILABLE};
    use crate::ui::test_support::{
        DashboardFixture, grouped_row, loaded_page, off_page_row, render, serde_row,
    };

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

    /// The bar counts the selection and the repositories it spans, and
    /// offers every action.
    #[test]
    fn the_bar_counts_the_selection_and_offers_every_action() {
        fn Fixture() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected: vec![grouped_row(), serde_row()],
                    ActionBar { onrequest: move |_| {} }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains("2 selected"), "{html}");
        assert!(html.contains("2 repositories"), "{html}");
        assert!(html.contains("Request rebase"), "{html}");
        assert!(html.contains("Update branch"), "{html}");
        assert!(html.contains("Merge selected"), "{html}");
        assert!(!html.contains("disabled"), "{html}");
        assert!(!html.contains("action-note"), "{html}");

        fn OneRepository() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected: vec![serde_row(), off_page_row()],
                    ActionBar { onrequest: move |_| {} }
                }
            }
        }
        let html = render(OneRepository);
        assert!(html.contains("2 selected"), "{html}");
        assert!(html.contains("1 repository"), "{html}");
    }

    /// When "select all matching" could not take every row the filter
    /// matched, the bar says how many it took and why.
    #[test]
    fn the_bar_says_when_the_batch_limit_cut_the_selection_short() {
        fn Fixture() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected: vec![grouped_row(), serde_row()],
                    capped_from: Some(52),
                    ActionBar { onrequest: move |_| {} }
                }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains("the first 2 of 52 matching; a batch takes at most 100"),
            "{html}"
        );
        assert!(!html.contains("disabled"), "{html}");
    }

    /// A selection a batch cannot take is not offered one: rows picked one by
    /// one on later pages can push a selection past the limit.
    #[test]
    fn a_selection_over_the_batch_limit_has_its_actions_withheld() {
        fn Fixture() -> Element {
            let selected = (0..=MAX_BATCH_TARGETS as u64)
                .map(|number| PrRecord {
                    id: format!("8#{number}"),
                    number,
                    ..off_page_row()
                })
                .collect();
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected,
                    ActionBar { onrequest: move |_| {} }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains("101 selected"), "{html}");
        assert!(
            html.contains("a batch takes at most 100 pull requests; deselect 1"),
            "{html}"
        );
        assert_eq!(
            html.matches("disabled").count(),
            3,
            "every action is withheld: {html}"
        );
    }

    /// A deployment without a user PAT cannot post `@dependabot rebase`; the
    /// service says so, and the bar withholds that one action with the reason,
    /// leaving merge and update branch, which run as the App, on offer. Until
    /// the service has said either way — the answer still in flight, or not
    /// had — nothing is withheld: the service guards the command itself.
    #[test]
    fn a_rebase_is_withheld_with_its_reason_only_once_the_service_says_it_is_off() {
        #[component]
        fn Fixture(capabilities: CapabilitiesStatus) -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected: vec![grouped_row(), serde_row()],
                    capabilities,
                    ActionBar { onrequest: move |_| {} }
                }
            }
        }
        let render = |capabilities: CapabilitiesStatus| {
            let mut dom = VirtualDom::new_with_props(Fixture, FixtureProps { capabilities });
            dom.rebuild_in_place();
            dioxus::ssr::render(&dom)
        };

        let off = render(CapabilitiesStatus::Loaded(Capabilities {
            rebase_enabled: false,
        }));
        assert!(off.contains(REBASE_UNAVAILABLE), "{off}");
        assert_eq!(
            off.matches("disabled").count(),
            1,
            "the rebase alone is withheld: {off}"
        );
        assert!(off.contains(r#"rebase-button" disabled=true"#), "{off}");

        let on = render(CapabilitiesStatus::Loaded(Capabilities {
            rebase_enabled: true,
        }));
        assert!(!on.contains("disabled"), "{on}");
        assert!(!on.contains(REBASE_UNAVAILABLE), "{on}");

        for unknown in [
            CapabilitiesStatus::Loading,
            CapabilitiesStatus::Failed("Restate is unavailable".to_owned()),
        ] {
            let html = render(unknown);
            assert!(!html.contains("disabled"), "{html}");
            assert!(!html.contains(REBASE_UNAVAILABLE), "{html}");
        }
    }
}
