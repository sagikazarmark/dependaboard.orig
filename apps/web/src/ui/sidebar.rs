//! The filter sidebar: search, the facets, the ranked labels, and the
//! repository tree, each toggling one dimension of the dashboard's filter.
//! The counts come from the summary, scoped to the filter in force minus the
//! facet's own dimension, so each says how many pull requests choosing it
//! would show. Until the summary is in hand there are no counts to show, and
//! the sidebar says so rather than show zeros, as the table does for its rows.

use dependaboard_core::{CheckStatus, FacetCounts, UpdateType};
use dioxus::prelude::*;

use crate::components::button::{Button, ButtonSize};
use crate::components::loading::{Loading, LoadingSize};
use crate::ui::dashboard_state::{SummaryStatus, use_dashboard};
use crate::ui::filters::{FacetButton, FilterSection, LabelFacets, filter_count};
use crate::ui::format::{status_class, status_label, update_class};
use crate::ui::repo_tree::RepoTree;
use crate::ui::search_box::SearchBox;

/// The sidebar; `open` says whether it is shown or folded away. The search
/// and the filter's own count are always there, since they belong to the
/// filter; below them stands whatever the summary has come to: still being
/// read, failed with a way to ask again, or the facets.
#[component]
pub(crate) fn Sidebar(open: Signal<bool>) -> Element {
    let mut state = use_dashboard();
    let status = state.summary.read();
    let active_filter_count = filter_count(&state.filter());
    rsx! {
        aside {
            class: if open() { "sidebar" } else { "sidebar sidebar-closed" },
            SearchBox {}
            div { class: "filter-meta",
                span { class: "mono muted", "{active_filter_count} active" }
                button {
                    disabled: active_filter_count == 0,
                    onclick: move |_| state.clear_filters(),
                    "clear"
                }
            }
            match &*status {
                SummaryStatus::Failed(error) => rsx! {
                    div { class: "empty-state error-state sidebar-state",
                        strong { "The facets could not be loaded" }
                        code { "{error}" }
                        Button { size: ButtonSize::Sm, onclick: move |_| state.reload(), "Retry" }
                    }
                },
                SummaryStatus::Loading => rsx! {
                    div { class: "loading-state sidebar-state",
                        Loading { size: LoadingSize::Sm }
                        "Reading the facets"
                    }
                },
                SummaryStatus::Loaded(summary) => rsx! {
                    Facets { facets: summary.facets.clone() }
                },
            }
        }
    }
}

/// The facet sections over the summary's counts, each marking what the
/// filter in force has set.
#[component]
fn Facets(facets: FacetCounts) -> Element {
    let mut state = use_dashboard();
    rsx! {
        FilterSection { title: "Check rollup" }
        div { class: "facet-list",
            for status in CheckStatus::ALL {
                FacetButton {
                    key: "check-{status}",
                    label: status_label(status),
                    count: facets.check_count(status),
                    active: state.filter().check_statuses.contains(&status),
                    tone: status_class(status),
                    onclick: move |_| state.toggle_filter(|filter| &mut filter.check_statuses, status),
                }
            }
        }

        FilterSection { title: "Update type" }
        div { class: "facet-list",
            for update_type in UpdateType::ALL {
                FacetButton {
                    key: "type-{update_type}",
                    label: update_type.to_string(),
                    count: facets.update_type_count(update_type),
                    active: state.filter().update_types.contains(&update_type),
                    tone: update_class(update_type),
                    onclick: move |_| state.toggle_filter(|filter| &mut filter.update_types, update_type),
                }
            }
        }

        FilterSection { title: "Labels" }
        LabelFacets {
            labels: facets.labels.clone(),
            active: state.filter().labels.clone(),
            ontoggle: move |label| state.toggle_filter(|filter| &mut filter.labels, label),
        }

        FilterSection { title: "Accounts & repositories" }
        RepoTree { repositories: facets.repositories.clone() }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::PrFilter;

    use super::*;
    use crate::ui::dashboard_state::SummaryStatus;
    use crate::ui::test_support::{DashboardFixture, loaded_summary, render};

    #[test]
    fn the_sidebar_marks_the_facets_and_repositories_the_filter_has_set() {
        fn Fixture() -> Element {
            let filter = PrFilter {
                check_statuses: vec![CheckStatus::Failure],
                repos: vec!["acme/web".to_owned()],
                ..Default::default()
            };
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { filter, summary: SummaryStatus::Loaded(loaded_summary()),
                    Sidebar { open }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains(r#"<aside class="sidebar">"#), "{html}");
        assert!(html.contains("2 active"), "{html}");
        assert_eq!(html.matches(r#"class="facet active""#).count(), 1, "{html}");
        assert!(
            html.contains(r#"<span>Failing</span><code>31</code>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<span>Passing</span><code>21</code>"#),
            "{html}"
        );
        assert!(html.contains("dependencies <span>52</span>"), "{html}");
        assert_eq!(
            html.matches(r#"class="repo-filter active""#).count(),
            1,
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="owner-name">acme</span>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<span class="repo-name">web</span><code>21</code>"#),
            "{html}"
        );
    }

    /// While the summary is still being read there are no counts to show,
    /// and a zero would be a claim; the sidebar says it is reading instead,
    /// as the table does. The search and the filter's own count stand, since
    /// they belong to the filter, not the summary.
    #[test]
    fn a_folded_sidebar_says_it_is_reading_while_the_summary_is_still_loading() {
        fn Fixture() -> Element {
            let open = use_signal(|| false);
            rsx! {
                DashboardFixture { Sidebar { open } }
            }
        }
        let html = render(Fixture);

        assert!(
            html.contains(r#"<aside class="sidebar sidebar-closed">"#),
            "{html}"
        );
        assert!(html.contains("0 active"), "{html}");
        assert!(html.contains("Reading the facets"), "{html}");
        assert!(!html.contains("<code>0</code>"), "{html}");
        assert!(!html.contains(r#"class="facet"#), "{html}");
        assert!(!html.contains("Check rollup"), "{html}");
        assert!(!html.contains("repo-filter"), "{html}");
        assert!(!html.contains("owner-group"), "{html}");
        assert!(!html.contains("Retry"), "{html}");
    }

    /// A summary that could not be read is said so, with the reason and a
    /// way to ask again, rather than shown as a dashboard with nothing in it.
    #[test]
    fn a_sidebar_whose_summary_failed_says_so_and_offers_a_retry() {
        fn Fixture() -> Element {
            let open = use_signal(|| true);
            rsx! {
                DashboardFixture { summary: SummaryStatus::Failed("the store is away".to_owned()),
                    Sidebar { open }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains("The facets could not be loaded"), "{html}");
        assert!(html.contains("<code>the store is away</code>"), "{html}");
        assert!(html.contains(">Retry<"), "{html}");
        assert!(!html.contains("Reading the facets"), "{html}");
        assert!(!html.contains("<code>0</code>"), "{html}");
        assert!(!html.contains(r#"class="facet"#), "{html}");
        assert!(!html.contains("repo-filter"), "{html}");
    }
}
