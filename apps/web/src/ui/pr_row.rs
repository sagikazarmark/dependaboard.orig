//! The pull request row of the dashboard table.

use dependaboard_core::{PrRecord, ROW_LABEL_LIMIT};
use dioxus::prelude::*;

use crate::ui::format::{relative_time, status_class, status_label, update_class, version_label};

/// One row; `now` is the dashboard's clock, which the row's age and staleness
/// are read against. `oncheck` receives the row when its box is clicked,
/// `onopen` when the row itself is; `onfilter_dependency` receives the
/// dependency's name when it is clicked, on a row that updates one dependency.
#[component]
pub(crate) fn PrRow(
    row: PrRecord,
    checked: bool,
    now: u64,
    oncheck: EventHandler<PrRecord>,
    onopen: EventHandler<PrRecord>,
    onfilter_dependency: EventHandler<String>,
) -> Element {
    let picked = row.clone();
    let opened = row.clone();
    let dependency = row.dependency.clone();
    let versions = version_label(row.from_version.as_deref(), row.to_version.as_deref());
    let stale = row.is_stale(now);
    let row_class = match (checked, stale) {
        (true, true) => "pr-grid pr-row selected stale",
        (true, false) => "pr-grid pr-row selected",
        (false, true) => "pr-grid pr-row stale",
        (false, false) => "pr-grid pr-row",
    };
    rsx! {
        div { class: row_class, onclick: move |_| onopen.call(opened.clone()),
            button {
                class: if checked { "selection-box checked" } else { "selection-box" },
                onclick: move |event| { event.stop_propagation(); oncheck.call(picked.clone()); },
                if checked { "x" }
            }
            code { class: "pr-number", "#{row.number}" }
            div { class: "dependency-cell",
                div {
                    match dependency {
                        Some(name) => rsx! {
                            button {
                                class: "dependency-filter",
                                aria_label: "show every {name} update",
                                onclick: {
                                    let name = name.clone();
                                    move |event: MouseEvent| { event.stop_propagation(); onfilter_dependency.call(name.clone()); }
                                },
                                strong { "{name}" }
                            }
                        },
                        None => rsx! { strong { "{row.dependencies.len()} dependencies" } },
                    }
                    span { "{versions}" }
                    span { class: "update-chip {update_class(row.update_type)}", "{row.update_type}" }
                }
                small { "{row.title}" }
            }
            code { class: "repository-cell", span { "{row.owner}/" } "{row.repo}" }
            div { class: "check-cell",
                span { class: "check-dot {status_class(row.check_status)}" }
                span { "{status_label(row.check_status)}" }
            }
            div { class: "row-labels",
                for label in row.labels.iter().take(ROW_LABEL_LIMIT) { span { "{label}" } }
                if row.labels.len() > ROW_LABEL_LIMIT { span { "+{row.labels.len() - ROW_LABEL_LIMIT}" } }
            }
            time { class: "right mono", "{relative_time(now, row.updated_at)}" }
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::ui::test_support::{FIXTURE_NOW, GROUPED_ROW_TITLE, grouped_row, serde_row};

    fn GroupedRowFixture() -> Element {
        rsx! {
            PrRow {
                row: grouped_row(),
                checked: false,
                now: FIXTURE_NOW,
                oncheck: move |_| {},
                onopen: move |_| {},
                onfilter_dependency: move |_| {},
            }
        }
    }

    #[test]
    fn grouped_row_does_not_repeat_a_long_title_as_its_version() {
        let mut dom = VirtualDom::new(GroupedRowFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        assert_eq!(html.matches(GROUPED_ROW_TITLE).count(), 1, "{html}");
        assert!(html.contains("<strong>3 dependencies</strong><span>group update</span>"));
    }

    fn SerdeRowFixture() -> Element {
        rsx! {
            PrRow {
                row: serde_row(),
                checked: false,
                now: FIXTURE_NOW,
                oncheck: move |_| {},
                onopen: move |_| {},
                onfilter_dependency: move |_| {},
            }
        }
    }

    /// The dependency's name is the way to every other pull request bumping it, so a
    /// row with one dependency offers it as a filter. A grouped row has no one name to
    /// filter by, so it shows its count as plain text.
    #[test]
    fn a_single_dependency_is_offered_as_a_filter_and_a_group_is_not() {
        let mut dom = VirtualDom::new(SerdeRowFixture);
        dom.rebuild_in_place();
        let single = dioxus::ssr::render(&dom);
        let mut dom = VirtualDom::new(GroupedRowFixture);
        dom.rebuild_in_place();
        let grouped = dioxus::ssr::render(&dom);

        assert!(
            single.contains(
                r#"<button class="dependency-filter" aria-label="show every serde update"><strong>serde</strong></button>"#
            ),
            "{single}"
        );
        assert!(!grouped.contains("dependency-filter"), "{grouped}");
    }

    fn ManyLabelsFixture() -> Element {
        let mut row = grouped_row();
        row.labels = ["dependencies", "rust", "security", "blocked"]
            .map(str::to_owned)
            .to_vec();
        rsx! {
            PrRow {
                row,
                checked: false,
                now: FIXTURE_NOW,
                oncheck: move |_| {},
                onopen: move |_| {},
                onfilter_dependency: move |_| {},
            }
        }
    }

    #[test]
    fn a_row_shows_the_first_labels_and_folds_the_rest_into_a_count() {
        let mut dom = VirtualDom::new(ManyLabelsFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        assert!(
            html.contains(
                r#"<div class="row-labels"><span>dependencies</span><span>rust</span><span>+2</span></div>"#
            ),
            "{html}"
        );
    }
}
