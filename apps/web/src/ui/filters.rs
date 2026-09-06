//! The sidebar filter controls and the active-filter chips above the table.

use dependaboard_core::{LABEL_FACET_LIMIT, LabelFacet, PrFilter};
use dioxus::prelude::*;

use crate::ui::dashboard_state::use_dashboard;

#[component]
pub(crate) fn FilterSection(title: &'static str) -> Element {
    rsx! { h2 { class: "filter-title", "{title}" } }
}

#[component]
pub(crate) fn FacetButton(
    label: String,
    count: u64,
    active: bool,
    tone: &'static str,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        button {
            class: if active { "facet active" } else { "facet" },
            onclick,
            span { class: "facet-dot {tone}" }
            span { "{label}" }
            code { "{count}" }
        }
    }
}

/// The most common labels as chips, cut off at [`LABEL_FACET_LIMIT`]. `labels`
/// arrives already ranked by the store (descending count, then name) and is
/// rendered in that order.
#[component]
pub(crate) fn LabelFacets(
    labels: Vec<LabelFacet>,
    active: Vec<String>,
    ontoggle: EventHandler<String>,
) -> Element {
    rsx! {
        div { class: "label-facets",
            for facet in labels.into_iter().take(LABEL_FACET_LIMIT) {
                {
                    let is_active = active.contains(&facet.label);
                    let label_value = facet.label.clone();
                    rsx! {
                        button {
                            key: "label-{facet.label}",
                            class: if is_active { "label-filter active" } else { "label-filter" },
                            onclick: move |_| ontoggle.call(label_value.clone()),
                            "{facet.label} " span { "{facet.count}" }
                        }
                    }
                }
            }
        }
    }
}

/// The chips above the table, one per filter in force, each removing its
/// filter; and a clear-all.
#[component]
pub(crate) fn ActiveFilters() -> Element {
    let mut state = use_dashboard();
    let value = state.filter().clone();
    rsx! {
        div { class: "active-filters",
            if value.needs_attention {
                FilterChip { kind: "view", label: "needs attention", onclick: move |_| state.update_filter(|filter| filter.needs_attention = false) }
            }
            for status in value.check_statuses {
                FilterChip { key: "chip-check-{status}", kind: "checks", label: status.to_string(), onclick: move |_| state.toggle_filter(|filter| &mut filter.check_statuses, status) }
            }
            for update_type in value.update_types {
                FilterChip { key: "chip-type-{update_type}", kind: "type", label: update_type.to_string(), onclick: move |_| state.toggle_filter(|filter| &mut filter.update_types, update_type) }
            }
            for repo in value.repos {
                {
                    let repo_value = repo.clone();
                    rsx! { FilterChip { key: "chip-repo-{repo}", kind: "repo", label: repo, onclick: move |_| state.toggle_filter(|filter| &mut filter.repos, repo_value.clone()) } }
                }
            }
            for label in value.labels {
                {
                    let label_value = label.clone();
                    rsx! { FilterChip { key: "chip-label-{label}", kind: "label", label, onclick: move |_| state.toggle_filter(|filter| &mut filter.labels, label_value.clone()) } }
                }
            }
            if let Some(dependency) = value.dependency {
                FilterChip { kind: "dependency", label: dependency, onclick: move |_| state.update_filter(|filter| filter.dependency = None) }
            }
            button { class: "clear-all", onclick: move |_| state.clear_filters(), "clear all" }
        }
    }
}

#[component]
fn FilterChip(kind: &'static str, label: String, onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button { class: "filter-chip", onclick,
            span { "{kind}" }
            "{label}"
            b { "x" }
        }
    }
}

/// How many filters the dashboard's controls have set: one per chip, plus the
/// search box for the query.
pub(crate) fn filter_count(filter: &PrFilter) -> usize {
    usize::from(filter.query.is_some())
        + filter.repos.len()
        + filter.update_types.len()
        + filter.check_statuses.len()
        + filter.labels.len()
        + usize::from(filter.dependency.is_some())
        + usize::from(filter.needs_attention)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{CheckStatus, UpdateType};

    use super::*;
    use crate::ui::test_support::{DashboardFixture, render};

    fn LabelFacetsFixture() -> Element {
        // Nine labels already ranked by the store, deliberately not in
        // alphabetical order, so the component can only pass by keeping the
        // sequence it was given and cutting it at eight.
        let labels = [
            ("rust", 9),
            ("go", 7),
            ("security", 7),
            ("dependencies", 5),
            ("python", 4),
            ("java", 3),
            ("javascript", 2),
            ("blocked", 1),
            ("actions", 1),
        ]
        .into_iter()
        .map(|(label, count)| LabelFacet {
            label: label.to_owned(),
            count,
        })
        .collect();
        rsx! {
            LabelFacets {
                labels,
                active: vec!["go".to_owned()],
                ontoggle: move |_| {},
            }
        }
    }

    #[test]
    fn label_facets_keep_the_store_ranking_and_show_the_top_eight() {
        let mut dom = VirtualDom::new(LabelFacetsFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        let positions = [
            "rust <span>9</span>",
            "go <span>7</span>",
            "security <span>7</span>",
            "dependencies <span>5</span>",
            "python <span>4</span>",
            "java <span>3</span>",
            "javascript <span>2</span>",
            "blocked <span>1</span>",
        ]
        .map(|chip| {
            html.find(chip)
                .unwrap_or_else(|| panic!("{chip} missing in {html}"))
        });
        assert!(positions.is_sorted(), "{html}");
        assert!(!html.contains("actions"), "{html}");
        assert!(
            html.contains(r#"<button class="label-filter active">go "#),
            "{html}"
        );
    }

    /// The count in the sidebar promises controls the user can clear: one chip
    /// per facet value, the dependency's included, plus the search box for the
    /// query.
    #[test]
    fn the_active_filter_count_covers_every_filter_the_dashboard_can_set() {
        fn Fixture() -> Element {
            rsx! {
                DashboardFixture { filter: full_filter(), ActiveFilters {} }
            }
        }
        let html = render(Fixture);

        assert_eq!(html.matches(r#"class="filter-chip""#).count(), 8, "{html}");
        assert!(
            html.contains(
                r#"<button class="filter-chip"><span>dependency</span>serde<b>x</b></button>"#
            ),
            "{html}"
        );
        assert_eq!(
            filter_count(&full_filter()),
            9,
            "the chips and the search box"
        );
    }

    fn full_filter() -> PrFilter {
        PrFilter {
            query: Some("serde".to_owned()),
            repos: vec!["acme/api".to_owned(), "acme/web".to_owned()],
            update_types: vec![UpdateType::Major],
            check_statuses: vec![CheckStatus::Failure, CheckStatus::None],
            labels: vec!["rust".to_owned()],
            dependency: Some("serde".to_owned()),
            needs_attention: true,
        }
    }
}
