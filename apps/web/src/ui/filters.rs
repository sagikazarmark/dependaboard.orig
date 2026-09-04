//! The sidebar filter controls and the active-filter chips above the table.

use dependaboard_core::{LabelFacet, PrFilter};
use dioxus::prelude::*;

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

/// How many of the ranked labels the sidebar shows before cutting off.
const LABEL_FACET_LIMIT: usize = 8;

/// The most common labels as chips. `labels` arrives already ranked by the
/// store (descending count, then name) and is rendered in that order.
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

#[component]
pub(crate) fn ActiveFilters(
    mut filter: Signal<PrFilter>,
    mut cursor: Signal<Option<String>>,
) -> Element {
    let value = filter();
    rsx! {
        div { class: "active-filters",
            if value.needs_attention {
                FilterChip { kind: "view", label: "needs attention", onclick: move |_| { filter.write().needs_attention = false; cursor.set(None); } }
            }
            for status in value.check_statuses {
                FilterChip { key: "chip-check-{status}", kind: "checks", label: status.to_string(), onclick: move |_| { toggle_value(&mut filter.write().check_statuses, status); cursor.set(None); } }
            }
            for update_type in value.update_types {
                FilterChip { key: "chip-type-{update_type}", kind: "type", label: update_type.to_string(), onclick: move |_| { toggle_value(&mut filter.write().update_types, update_type); cursor.set(None); } }
            }
            for repo in value.repos {
                {
                    let repo_value = repo.clone();
                    rsx! { FilterChip { key: "chip-repo-{repo}", kind: "repo", label: repo, onclick: move |_| { toggle_value(&mut filter.write().repos, repo_value.clone()); cursor.set(None); } } }
                }
            }
            for label in value.labels {
                {
                    let label_value = label.clone();
                    rsx! { FilterChip { key: "chip-label-{label}", kind: "label", label, onclick: move |_| { toggle_value(&mut filter.write().labels, label_value.clone()); cursor.set(None); } } }
                }
            }
            button { class: "clear-all", onclick: move |_| { filter.set(PrFilter::default()); cursor.set(None); }, "clear all" }
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

pub(crate) fn filter_count(filter: &PrFilter) -> usize {
    usize::from(filter.query.is_some())
        + usize::from(filter.owner.is_some())
        + filter.repos.len()
        + filter.update_types.len()
        + filter.check_statuses.len()
        + filter.labels.len()
        + usize::from(filter.dependency.is_some())
        + usize::from(filter.needs_attention)
}

pub(crate) fn toggle_value<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if let Some(index) = values.iter().position(|candidate| candidate == &value) {
        values.remove(index);
    } else {
        values.push(value);
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;

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
}
