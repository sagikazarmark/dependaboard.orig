//! The state the dashboard's components share: the filter in force, the page
//! cursor, the selection, and the read model's answers.
//!
//! The filter, cursor, and selection are only changed through the methods
//! here, so that a filter change always restarts paging and drops the
//! selection, whichever control made it, and the selection is always read
//! against the page in force.

use std::collections::BTreeSet;

use dependaboard_core::{DashboardPage, DashboardSummary, PrFilter, PrTarget};
use dioxus::prelude::*;

use crate::ui::{pr_target, user_facing};

/// What the dashboard has heard from the read model in answer to one
/// question.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Remote<T> {
    /// The first answer has not arrived yet.
    Loading,
    /// The read model could not be loaded; the text is what the user sees.
    Failed(String),
    Loaded(T),
}

impl<T: Clone> Remote<T> {
    pub(crate) fn from_resource(value: Option<&Result<T, ServerFnError>>) -> Self {
        match value {
            None => Self::Loading,
            Some(Err(error)) => Self::Failed(user_facing(error)),
            Some(Ok(answer)) => Self::Loaded(answer.clone()),
        }
    }

    pub(crate) fn loaded(&self) -> Option<&T> {
        match self {
            Self::Loaded(answer) => Some(answer),
            _ => None,
        }
    }
}

/// The rows for the filter and cursor in force.
pub(crate) type PageStatus = Remote<DashboardPage>;

/// The facets and freshness for the filter in force. Asked for once per
/// filter: moving to the next page does not ask again.
pub(crate) type SummaryStatus = Remote<DashboardSummary>;

/// The dashboard's shared state, provided as a context by [`Dashboard`] and
/// read by its components through [`use_dashboard`].
///
/// [`Dashboard`]: crate::ui::dashboard::Dashboard
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct DashboardState {
    filter: Signal<PrFilter>,
    cursor: Signal<Option<String>>,
    /// The ids of the rows picked for a bulk action.
    selected: Signal<BTreeSet<String>>,
    /// The read model's rows for the filter and cursor in force.
    pub(crate) page: ReadSignal<PageStatus>,
    /// The read model's facets and freshness for the filter in force.
    pub(crate) summary: ReadSignal<SummaryStatus>,
    /// Asks the read model again for the same filter and cursor.
    pub(crate) reload: Callback<()>,
}

impl DashboardState {
    /// Provides the state to the calling component's subtree. `page` and
    /// `summary` are boxed once, here, since every conversion to a
    /// [`ReadSignal`] takes a slot in the scope for as long as the scope
    /// lives.
    pub(crate) fn provide(
        filter: Signal<PrFilter>,
        cursor: Signal<Option<String>>,
        selected: Signal<BTreeSet<String>>,
        page: impl Into<ReadSignal<PageStatus>>,
        summary: impl Into<ReadSignal<SummaryStatus>>,
        reload: Callback<()>,
    ) -> Self {
        use_context_provider(|| Self {
            filter,
            cursor,
            selected,
            page: page.into(),
            summary: summary.into(),
            reload,
        })
    }

    /// The filter in force. Reading it in a component subscribes the
    /// component to filter changes.
    pub(crate) fn filter(&self) -> ReadableRef<'_, Signal<PrFilter>> {
        self.filter.read()
    }

    /// The cursor of the page in force; `None` is the first page.
    pub(crate) fn cursor(&self) -> ReadableRef<'_, Signal<Option<String>>> {
        self.cursor.read()
    }

    /// Applies `change` to the filter, restarts paging, and drops the
    /// selection, which belonged to the rows the old filter showed.
    pub(crate) fn update_filter(&mut self, change: impl FnOnce(&mut PrFilter)) {
        change(&mut self.filter.write());
        self.cursor.set(None);
        self.selected.write().clear();
    }

    /// Moves to `filter` and `cursor` together, as the browser's back and
    /// forward do, dropping the selection, which was made on the page being
    /// left. The cursor is kept as given: it belongs to `filter`'s paging.
    /// Only what differs is written, so landing where the dashboard already
    /// is does not ask the read model again.
    pub(crate) fn navigate(&mut self, filter: PrFilter, cursor: Option<String>) {
        if *self.filter.peek() != filter {
            self.filter.set(filter);
        }
        if *self.cursor.peek() != cursor {
            self.cursor.set(cursor);
        }
        if !self.selected.peek().is_empty() {
            self.selected.write().clear();
        }
    }

    /// Adds `value` to the facet `facet` selects, or removes it if it is
    /// already there, as [`Self::update_filter`] does.
    pub(crate) fn toggle_filter<T: PartialEq>(
        &mut self,
        facet: impl FnOnce(&mut PrFilter) -> &mut Vec<T>,
        value: T,
    ) {
        self.update_filter(|filter| toggle_value(facet(filter), value));
    }

    /// Selects every one of `repos` (as `owner/repo`) in the repository
    /// filter, or deselects every one of them if `select` is false, as
    /// [`Self::update_filter`] does. A repository already as asked is left
    /// where it is, so the rest of the filter keeps its order.
    pub(crate) fn select_repos(&mut self, repos: impl IntoIterator<Item = String>, select: bool) {
        self.update_filter(|filter| {
            for repo in repos {
                let present = filter.repos.iter().position(|candidate| candidate == &repo);
                match (present, select) {
                    (None, true) => filter.repos.push(repo),
                    (Some(index), false) => {
                        filter.repos.remove(index);
                    }
                    _ => {}
                }
            }
        });
    }

    /// Drops every filter, as [`Self::update_filter`] does.
    pub(crate) fn clear_filters(&mut self) {
        self.update_filter(|filter| *filter = PrFilter::default());
    }

    /// Moves to the page after `cursor`, dropping the selection with the rows
    /// it belonged to.
    pub(crate) fn load_next(&mut self, cursor: String) {
        self.cursor.set(Some(cursor));
        self.selected.write().clear();
    }

    /// Whether the row `id` is selected.
    pub(crate) fn is_selected(&self, id: &str) -> bool {
        self.selected.read().contains(id)
    }

    pub(crate) fn selected_count(&self) -> usize {
        self.selected.read().len()
    }

    /// Whether the page in force has rows and every one of them is selected.
    pub(crate) fn all_visible_selected(&self) -> bool {
        let page = self.page.read();
        let selected = self.selected.read();
        page.loaded().is_some_and(|page| {
            !page.rows.is_empty() && page.rows.iter().all(|row| selected.contains(&row.id))
        })
    }

    /// Selects the row `id`, or deselects it if it is selected.
    pub(crate) fn toggle_selected(&mut self, id: String) {
        let mut selected = self.selected.write();
        if !selected.insert(id.clone()) {
            selected.remove(&id);
        }
    }

    /// Selects every row of the page in force, or deselects them all if every
    /// one is already selected.
    pub(crate) fn toggle_visible(&mut self) {
        let clear = self.all_visible_selected();
        let page = self.page.read();
        let Some(page) = page.loaded() else {
            return;
        };
        let mut selected = self.selected.write();
        for row in &page.rows {
            if clear {
                selected.remove(&row.id);
            } else {
                selected.insert(row.id.clone());
            }
        }
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selected.write().clear();
    }

    /// The selected rows of the page in force as bulk-action targets, in page
    /// order. A selected id the page no longer shows is not among them.
    pub(crate) fn selected_targets(&self) -> Vec<PrTarget> {
        let page = self.page.read();
        let selected = self.selected.read();
        page.loaded()
            .map(|page| {
                page.rows
                    .iter()
                    .filter(|row| selected.contains(&row.id))
                    .map(pr_target)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn toggle_value<T: PartialEq>(values: &mut Vec<T>, value: T) {
    if let Some(index) = values.iter().position(|candidate| candidate == &value) {
        values.remove(index);
    } else {
        values.push(value);
    }
}

/// The dashboard state provided by the nearest [`Dashboard`].
///
/// [`Dashboard`]: crate::ui::dashboard::Dashboard
pub(crate) fn use_dashboard() -> DashboardState {
    use_context()
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::CheckStatus;
    use dioxus::core::consume_context_from_scope;

    use super::*;
    use crate::ui::test_support::{loaded_page, loaded_summary};

    /// A dashboard state on the app scope: the fixture page loaded, on its
    /// second page, with one row selected, so the tests can see both reset.
    fn Fixture() -> Element {
        DashboardState::provide(
            use_signal(PrFilter::default),
            use_signal(|| Some("page-2".to_owned())),
            use_signal(|| BTreeSet::from(["7#9".to_owned()])),
            use_signal(|| PageStatus::Loaded(loaded_page())),
            use_signal(|| SummaryStatus::Loaded(loaded_summary())),
            use_callback(|_| {}),
        );
        rsx! {}
    }

    fn mount() -> (VirtualDom, DashboardState) {
        let mut dom = VirtualDom::new(Fixture);
        dom.rebuild_in_place();
        let state = dom
            .in_runtime(|| consume_context_from_scope::<DashboardState>(ScopeId::APP))
            .expect("the fixture provides the dashboard state");
        (dom, state)
    }

    #[test]
    fn toggling_a_facet_adds_it_then_removes_it_restarting_paging_each_time() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.toggle_filter(|filter| &mut filter.check_statuses, CheckStatus::Failure);
            assert_eq!(state.filter().check_statuses, vec![CheckStatus::Failure]);
            assert_eq!(*state.cursor(), None);
            assert_eq!(state.selected_count(), 0);

            state.load_next("page-2".to_owned());
            state.toggle_filter(|filter| &mut filter.check_statuses, CheckStatus::Failure);
            assert!(state.filter().check_statuses.is_empty());
            assert_eq!(*state.cursor(), None);
        });
    }

    /// The owner checkbox selects or deselects several repositories at once.
    /// Selecting keeps a repository that is already selected in place and
    /// appends the others; deselecting removes only the ones named, so a
    /// repository under another owner is untouched. Both restart paging.
    #[test]
    fn selecting_several_repos_adds_the_missing_ones_and_deselecting_removes_only_them() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.update_filter(|filter| filter.repos = vec!["beta/api".to_owned()]);
            state.load_next("page-2".to_owned());

            state.select_repos(["acme/api".to_owned(), "acme/web".to_owned()], true);
            assert_eq!(state.filter().repos, ["beta/api", "acme/api", "acme/web"]);
            assert_eq!(*state.cursor(), None);

            state.select_repos(["acme/web".to_owned(), "acme/api".to_owned()], true);
            assert_eq!(
                state.filter().repos,
                ["beta/api", "acme/api", "acme/web"],
                "selecting again changes nothing"
            );

            state.load_next("page-2".to_owned());
            state.select_repos(["acme/api".to_owned(), "acme/web".to_owned()], false);
            assert_eq!(state.filter().repos, ["beta/api"]);
            assert_eq!(*state.cursor(), None);
        });
    }

    #[test]
    fn changing_or_clearing_the_filter_restarts_paging_and_drops_the_selection() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.update_filter(|filter| filter.needs_attention = true);
            assert!(state.filter().needs_attention);
            assert_eq!(*state.cursor(), None);
            assert_eq!(state.selected_count(), 0);

            state.load_next("page-2".to_owned());
            state.toggle_selected("7#9".to_owned());
            assert_eq!(*state.cursor(), Some("page-2".to_owned()));
            state.clear_filters();
            assert_eq!(*state.filter(), PrFilter::default());
            assert_eq!(*state.cursor(), None);
            assert_eq!(state.selected_count(), 0);
        });
    }

    #[test]
    fn a_row_toggles_in_and_out_of_the_selection() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            assert!(state.is_selected("7#9"));
            state.toggle_selected("7#9".to_owned());
            assert!(!state.is_selected("7#9"));
            state.toggle_selected("8#12".to_owned());
            assert!(state.is_selected("8#12"));
            assert_eq!(state.selected_count(), 1);
        });
    }

    /// Back and forward land on a filter and a cursor together: the cursor
    /// belongs to that filter's paging, so it is kept rather than reset, and
    /// the selection, made on the page being left, is dropped.
    #[test]
    fn navigating_sets_the_filter_and_cursor_together_and_drops_the_selection() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            let filter = PrFilter {
                check_statuses: vec![CheckStatus::Failure],
                ..PrFilter::default()
            };
            state.navigate(filter.clone(), Some("page-3".to_owned()));
            assert_eq!(*state.filter(), filter);
            assert_eq!(*state.cursor(), Some("page-3".to_owned()));
            assert_eq!(state.selected_count(), 0);

            state.toggle_selected("7#9".to_owned());
            state.navigate(PrFilter::default(), None);
            assert_eq!(*state.filter(), PrFilter::default());
            assert_eq!(*state.cursor(), None);
            assert_eq!(state.selected_count(), 0);
        });
    }

    /// With some of the page selected, "visible" selects the rest; only once
    /// every row is selected does it deselect them all.
    #[test]
    fn toggling_the_visible_rows_selects_them_all_before_it_clears_them() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            assert!(!state.all_visible_selected());
            state.toggle_visible();
            assert!(state.all_visible_selected());
            assert_eq!(state.selected_count(), 2);
            state.toggle_visible();
            assert_eq!(state.selected_count(), 0);
            assert!(!state.all_visible_selected());
        });
    }

    /// The targets are the selected rows of the page in force, in page
    /// order, so the confirmation names the head SHAs the batch will submit;
    /// a selected id the page no longer shows is not a target.
    #[test]
    fn the_targets_are_the_selected_rows_the_page_still_shows() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.toggle_selected("8#12".to_owned());
            state.toggle_selected("gone".to_owned());
            let targets = state.selected_targets();
            let numbers: Vec<u64> = targets.iter().map(|target| target.number).collect();
            assert_eq!(numbers, vec![9, 12]);
            assert_eq!(targets[1].expected_sha, "def456");
        });
    }
}
