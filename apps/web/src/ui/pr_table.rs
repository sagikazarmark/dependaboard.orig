//! The pull request table: the result bar, the filter chips, the rows for the
//! page in force, and the way to the next page. The header's box selects the
//! page; the result bar's link selects everything the filter matches, pages
//! beyond this one included.

use dependaboard_core::{DEFAULT_PAGE_SIZE, PrRecord};
use dioxus::prelude::*;

use crate::api::load_matching;
use crate::components::button::{Button, ButtonSize};
use crate::components::loading::{Loading, LoadingSize};
use crate::components::toast::use_toast;
use crate::ui::dashboard_state::{PageStatus, use_dashboard};
use crate::ui::filters::{ActiveFilters, filter_count};
use crate::ui::format::{Checkbox, pull_requests};
use crate::ui::pr_row::PrRow;
use crate::ui::{sticky, user_facing};

/// The table; `onopen` receives the row whose drawer the user asked for.
#[component]
pub(crate) fn PrTable(onopen: EventHandler<PrRecord>) -> Element {
    let mut state = use_dashboard();
    let toast = use_toast();
    // Whether "select all matching" is waiting on the read model.
    let mut selecting = use_signal(|| false);
    let status = state.page.read();
    let page = status.loaded();
    let rows: &[PrRecord] = page.map(|page| page.rows.as_slice()).unwrap_or_default();
    let total = page.map(|page| page.total).unwrap_or_default();
    let active_filter_count = filter_count(&state.filter());
    let header_box = Checkbox::of(state.visible_selected_count(), rows.len());
    let now = state.now();
    let select_matching = move |_| {
        let filter = state.filter().clone();
        selecting.set(true);
        spawn(async move {
            match load_matching(filter.clone()).await {
                Ok(matching) => state.select_matching(filter, matching),
                Err(error) => toast.error(
                    format!(
                        "Could not select the matching pull requests: {}",
                        user_facing(&error)
                    ),
                    sticky(),
                ),
            }
            selecting.set(false);
        });
    };
    rsx! {
        main { class: "content",
            div { class: "resultbar",
                span { class: "mono", "{pull_requests(total)}" }
                if !rows.is_empty() {
                    button {
                        disabled: selecting(),
                        onclick: select_matching,
                        if selecting() { "selecting..." } else { "select all {total} matching" }
                    }
                }
                span { class: "result-spacer" }
                span { class: "muted mono desktop-only", "updated recently first" }
            }
            if active_filter_count > 0 {
                ActiveFilters {}
            }
            div { class: "table-scroll",
                div { class: "pr-grid table-head",
                    button {
                        class: header_box.class(),
                        aria_label: "select every pull request on this page",
                        disabled: rows.is_empty(),
                        onclick: move |_| state.toggle_visible(),
                        "{header_box.mark()}"
                    }
                    span { "PR" }
                    span { "Dependency" }
                    span { "Repository" }
                    span { "Checks" }
                    span { "Labels" }
                    span { class: "right", "Updated" }
                }
                match &*status {
                    PageStatus::Failed(error) => rsx! {
                        div { class: "empty-state error-state",
                            strong { "The read model could not be loaded" }
                            code { "{error}" }
                            Button { size: ButtonSize::Sm, onclick: move |_| state.reload(), "Retry" }
                        }
                    },
                    PageStatus::Loading => rsx! {
                        div { class: "loading-state",
                            Loading { size: LoadingSize::Sm }
                            "Reading the projection"
                        }
                    },
                    PageStatus::Loaded(page) if page.rows.is_empty() => rsx! {
                        EmptyState { filtered: active_filter_count > 0 }
                    },
                    PageStatus::Loaded(page) => rsx! {
                        for row in &page.rows {
                            PrRow {
                                key: "{row.id}",
                                row: row.clone(),
                                checked: state.is_selected(&row.id),
                                now,
                                oncheck: move |row| state.toggle_selected(row),
                                onopen,
                                onfilter_dependency: move |name| state.update_filter(|filter| filter.dependency = Some(name)),
                            }
                        }
                    },
                }
                if let Some(next) = page.and_then(|page| page.next_cursor.clone()) {
                    div { class: "load-more",
                        Button {
                            size: ButtonSize::Sm,
                            class: "btn-ghost",
                            onclick: move |_| state.load_next(next.clone()),
                            "Load next {DEFAULT_PAGE_SIZE}"
                        }
                    }
                }
            }
        }
    }
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

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::ui::test_support::{DashboardFixture, grouped_row, loaded_page, render, serde_row};

    /// With one of the two rows selected, the header's box is mixed, and
    /// the result bar offers the whole filter, pages beyond this one included.
    #[test]
    fn the_table_lists_the_page_marks_the_selection_and_offers_the_next_page() {
        fn Fixture() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected: vec![serde_row()],
                    PrTable { onopen: move |_| {} }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains("52 pull requests"), "{html}");
        assert!(html.contains(">select all 52 matching</button>"), "{html}");
        assert_eq!(
            html.matches(r#"class="selection-box mixed""#).count(),
            1,
            "the header's box alone is mixed: {html}"
        );
        assert_eq!(
            html.matches(r#"class="pr-grid pr-row"#).count(),
            2,
            "{html}"
        );
        assert_eq!(
            html.matches(r#"class="pr-grid pr-row selected"#).count(),
            1,
            "{html}"
        );
        assert!(html.contains("Load next 50"), "{html}");
        assert!(!html.contains("active-filters"), "{html}");
    }

    /// The header's box stands for the page: unchecked while none of its rows
    /// is selected, checked once every one is.
    #[test]
    fn the_header_box_is_checked_once_every_visible_row_is_selected() {
        fn Unselected() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    PrTable { onopen: move |_| {} }
                }
            }
        }
        let none = render(Unselected);
        assert_eq!(
            none.matches(r#"class="selection-box""#).count(),
            3,
            "the header's box and both rows' are unchecked: {none}"
        );
        assert!(!none.contains("selection-box checked"), "{none}");
        assert!(!none.contains("selection-box mixed"), "{none}");

        fn Selected() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Loaded(loaded_page()),
                    selected: vec![grouped_row(), serde_row()],
                    PrTable { onopen: move |_| {} }
                }
            }
        }
        let all = render(Selected);
        assert_eq!(
            all.matches(r#"class="selection-box checked""#).count(),
            3,
            "the header's box and both rows' are checked: {all}"
        );
        assert!(!all.contains("selection-box mixed"), "{all}");
    }

    #[test]
    fn the_table_says_when_it_is_still_reading_and_when_the_read_failed() {
        fn Loading() -> Element {
            rsx! {
                DashboardFixture { PrTable { onopen: move |_| {} } }
            }
        }
        let loading = render(Loading);
        assert!(loading.contains("Reading the projection"), "{loading}");
        assert!(loading.contains("0 pull requests"), "{loading}");
        assert!(!loading.contains("Load next"), "{loading}");
        assert!(
            !loading.contains("select all"),
            "nothing to select yet: {loading}"
        );

        fn Failed() -> Element {
            rsx! {
                DashboardFixture {
                    page: PageStatus::Failed("the store is away".to_owned()),
                    PrTable { onopen: move |_| {} }
                }
            }
        }
        let failed = render(Failed);
        assert!(
            failed.contains("The read model could not be loaded"),
            "{failed}"
        );
        assert!(
            failed.contains("<code>the store is away</code>"),
            "{failed}"
        );
        assert!(failed.contains("Retry"), "{failed}");
        assert!(!failed.contains("Reading the projection"), "{failed}");
    }

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
