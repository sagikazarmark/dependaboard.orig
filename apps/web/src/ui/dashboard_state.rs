//! The state the dashboard's components share: the filter in force, the page
//! cursor, the selection, and the read model's answers.
//!
//! The filter, cursor, and selection are only changed through the methods
//! here, so that a filter change always restarts paging and drops the
//! selection, whichever control made it. The selection remembers each row as
//! it was picked, so it can span pages: "select all matching" fills it from
//! the read model, and loading the next page leaves it standing.

use std::collections::BTreeMap;

use dependaboard_core::{DashboardPage, DashboardSummary, PrFilter, PrRecord};
use dioxus::prelude::*;

use crate::ui::{repository_count, user_facing};

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

    /// The same answer with `f` applied to what was loaded; still loading, or
    /// failed, is passed on as it is.
    pub(crate) fn map<U>(&self, f: impl FnOnce(&T) -> U) -> Remote<U> {
        match self {
            Self::Loading => Remote::Loading,
            Self::Failed(error) => Remote::Failed(error.clone()),
            Self::Loaded(answer) => Remote::Loaded(f(answer)),
        }
    }
}

/// The rows for the filter and cursor in force.
pub(crate) type PageStatus = Remote<DashboardPage>;

/// The facets and freshness for the filter in force. Asked for once per
/// filter: moving to the next page does not ask again.
pub(crate) type SummaryStatus = Remote<DashboardSummary>;

/// The rows picked for a bulk action.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Selection {
    /// Each row by its id, as it was when picked.
    rows: BTreeMap<String, PrRecord>,
    /// How many rows the filter matched when "select all matching" filled
    /// the selection and the batch limit left some of them out; `None` while
    /// the selection holds everything it was asked to.
    capped_from: Option<u64>,
}

impl Selection {
    /// A selection of `rows`, as a test fixture starts with.
    #[cfg(all(test, feature = "server"))]
    pub(crate) fn of(rows: impl IntoIterator<Item = PrRecord>) -> Self {
        Self {
            rows: rows.into_iter().map(|row| (row.id.clone(), row)).collect(),
            capped_from: None,
        }
    }

    /// This selection as "select all matching" would have left it when the
    /// filter matched `total` rows and the batch limit kept only these.
    #[cfg(all(test, feature = "server"))]
    pub(crate) fn cut_short_from(self, total: u64) -> Self {
        Self {
            capped_from: Some(total),
            ..self
        }
    }
}

/// The dashboard's shared state, provided as a context by [`Dashboard`] and
/// read by its components through [`use_dashboard`].
///
/// [`Dashboard`]: crate::ui::dashboard::Dashboard
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct DashboardState {
    filter: Signal<PrFilter>,
    cursor: Signal<Option<String>>,
    /// The rows picked for a bulk action.
    selected: Signal<Selection>,
    /// The read model's rows for the filter and cursor in force.
    pub(crate) page: ReadSignal<PageStatus>,
    /// The read model's facets and freshness for the filter in force.
    pub(crate) summary: ReadSignal<SummaryStatus>,
    /// The dashboard's clock, in Unix seconds. It ticks on its own, so the
    /// relative times a component prints against it move without anything
    /// else happening.
    now: ReadSignal<u64>,
    /// Whether a manual sync is in flight: asked for, and not yet seen to
    /// reach the pull requests.
    syncing: Signal<bool>,
    /// Asks the read model again for the same filter and cursor.
    reload: Callback<()>,
}

impl DashboardState {
    /// Provides the state to the calling component's subtree. `page`,
    /// `summary`, and `now` are boxed once, here, since every conversion to a
    /// [`ReadSignal`] takes a slot in the scope for as long as the scope
    /// lives. No manual sync is in flight to begin with.
    pub(crate) fn provide(
        filter: Signal<PrFilter>,
        cursor: Signal<Option<String>>,
        selected: Signal<Selection>,
        page: impl Into<ReadSignal<PageStatus>>,
        summary: impl Into<ReadSignal<SummaryStatus>>,
        now: impl Into<ReadSignal<u64>>,
        reload: Callback<()>,
    ) -> Self {
        let syncing = use_signal(|| false);
        use_context_provider(|| Self {
            filter,
            cursor,
            selected,
            page: page.into(),
            summary: summary.into(),
            now: now.into(),
            syncing,
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

    /// The dashboard's clock, in Unix seconds. Reading it in a component
    /// subscribes the component to its ticks.
    pub(crate) fn now(&self) -> u64 {
        *self.now.read()
    }

    /// Whether a manual sync is in flight.
    pub(crate) fn syncing(&self) -> bool {
        *self.syncing.read()
    }

    /// A manual sync has been asked for. It is in flight until
    /// [`Self::end_sync`] says it is over: the live refresh has seen it
    /// reach the pull requests, has stopped waiting for it, or Restate did
    /// not take it.
    pub(crate) fn begin_sync(&mut self) {
        self.syncing.set(true);
    }

    /// The manual sync is over: it reached the pull requests, Restate did
    /// not take it, or the dashboard has stopped waiting for it.
    pub(crate) fn end_sync(&mut self) {
        if *self.syncing.peek() {
            self.syncing.set(false);
        }
    }

    /// Asks the read model again for the same filter and cursor. A manual
    /// sync in flight stays in flight: the rows reloading is not what it is
    /// waiting for — a sweep writes every repository before it reaches a
    /// pull request, and a retry after a failed read reloads them too.
    pub(crate) fn reload(&mut self) {
        self.reload.call(());
    }

    /// Applies `change` to the filter, restarts paging, and drops the
    /// selection, which belonged to the rows the old filter matched.
    pub(crate) fn update_filter(&mut self, change: impl FnOnce(&mut PrFilter)) {
        change(&mut self.filter.write());
        self.cursor.set(None);
        self.clear_selection();
    }

    /// Moves to `filter` and `cursor` together, as the browser's back and
    /// forward do. The cursor is kept as given: it belongs to `filter`'s
    /// paging. The selection goes with the filter: a different filter drops
    /// it, another page of the same filter keeps it. Only what differs is
    /// written, so landing where the dashboard already is does not ask the
    /// read model again.
    pub(crate) fn navigate(&mut self, filter: PrFilter, cursor: Option<String>) {
        if *self.filter.peek() != filter {
            self.filter.set(filter);
            self.clear_selection();
        }
        if *self.cursor.peek() != cursor {
            self.cursor.set(cursor);
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

    /// Moves to the page after `cursor`. The selection stays: it belongs to
    /// the filter, not the page, so rows picked across pages add up.
    pub(crate) fn load_next(&mut self, cursor: String) {
        self.cursor.set(Some(cursor));
    }

    /// Whether the row `id` is selected.
    pub(crate) fn is_selected(&self, id: &str) -> bool {
        self.selected.read().rows.contains_key(id)
    }

    pub(crate) fn selected_count(&self) -> usize {
        self.selected.read().rows.len()
    }

    /// How many repositories the selected rows span.
    pub(crate) fn selected_repository_count(&self) -> usize {
        repository_count(self.selected.read().rows.values())
    }

    /// How many of the page in force's rows are selected.
    pub(crate) fn visible_selected_count(&self) -> usize {
        let page = self.page.read();
        let selected = self.selected.read();
        page.loaded().map_or(0, |page| {
            page.rows
                .iter()
                .filter(|row| selected.rows.contains_key(&row.id))
                .count()
        })
    }

    /// Whether the page in force has rows and every one of them is selected.
    fn all_visible_selected(&self) -> bool {
        let page = self.page.read();
        page.loaded().is_some_and(|page| {
            !page.rows.is_empty() && self.visible_selected_count() == page.rows.len()
        })
    }

    /// Selects `row`, or deselects it if it is selected. A selection edited
    /// by hand is no longer "the first so many matching", so the cap note
    /// goes.
    pub(crate) fn toggle_selected(&mut self, row: PrRecord) {
        let mut selected = self.selected.write();
        if selected.rows.remove(&row.id).is_none() {
            selected.rows.insert(row.id.clone(), row);
        }
        selected.capped_from = None;
    }

    /// Selects every row of the page in force, or deselects them all if every
    /// one is already selected, forgetting the cap note as
    /// [`Self::toggle_selected`] does.
    pub(crate) fn toggle_visible(&mut self) {
        let clear = self.all_visible_selected();
        let page = self.page.read();
        let Some(page) = page.loaded() else {
            return;
        };
        let mut selected = self.selected.write();
        for row in &page.rows {
            if clear {
                selected.rows.remove(&row.id);
            } else {
                selected.rows.insert(row.id.clone(), row.clone());
            }
        }
        selected.capped_from = None;
    }

    /// Replaces the selection with `matching`, the read model's answer to
    /// "every row `filter` matches, as many as one batch takes". When its
    /// total says the filter matches more rows than it holds, the batch limit
    /// cut the set short, and the selection remembers the total so the bar
    /// can say so.
    ///
    /// The answer took a round trip, and the filter may have moved on
    /// meanwhile, dropping the selection with it; an answer for a filter no
    /// longer in force is ignored rather than put back a selection that does
    /// not belong to what is on screen.
    pub(crate) fn select_matching(&mut self, filter: PrFilter, matching: DashboardPage) {
        if *self.filter.peek() != filter {
            return;
        }
        let capped = (matching.rows.len() as u64) < matching.total;
        self.selected.set(Selection {
            rows: matching
                .rows
                .into_iter()
                .map(|row| (row.id.clone(), row))
                .collect(),
            capped_from: capped.then_some(matching.total),
        });
    }

    /// How many rows the filter matched when "select all matching" filled the
    /// selection and the batch limit left some of them out; `None` while the
    /// selection holds everything it was asked to.
    pub(crate) fn capped_from(&self) -> Option<u64> {
        self.selected.read().capped_from
    }

    pub(crate) fn clear_selection(&mut self) {
        if *self.selected.peek() != Selection::default() {
            self.selected.set(Selection::default());
        }
    }

    /// Takes `rows` out of the selection and leaves the rest standing. The
    /// rows a bulk action is queued with leave this way, whether that is the
    /// whole selection from the bar or the one pull request open in the
    /// drawer. Taking a row out edits the selection as
    /// [`Self::toggle_selected`] does, so the cap note goes with it.
    pub(crate) fn deselect(&mut self, rows: &[PrRecord]) {
        let picked = self.selected.peek();
        if !rows.iter().any(|row| picked.rows.contains_key(&row.id)) {
            return;
        }
        drop(picked);
        let mut selected = self.selected.write();
        for row in rows {
            selected.rows.remove(&row.id);
        }
        selected.capped_from = None;
    }

    /// The selected rows, newest update first as the table lists them. A row
    /// the page in force still shows is taken from the page, so the head SHA
    /// a bulk action captures is the one on screen; a row the page no longer
    /// shows is remembered as it was when picked.
    pub(crate) fn selected_rows(&self) -> Vec<PrRecord> {
        let page = self.page.read();
        let selected = self.selected.read();
        let mut rows: Vec<PrRecord> = selected
            .rows
            .values()
            .map(|remembered| {
                page.loaded()
                    .and_then(|page| page.rows.iter().find(|row| row.id == remembered.id))
                    .unwrap_or(remembered)
                    .clone()
            })
            .collect();
        rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        rows
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
    use crate::ui::test_support::{
        FIXTURE_NOW, grouped_row, loaded_page, loaded_summary, off_page_row, serde_row,
    };

    /// A dashboard state on the app scope: the fixture page loaded, on its
    /// second page, with one row selected, so the tests can see both reset.
    fn Fixture() -> Element {
        let reloads = use_context_provider(|| Signal::new(0u32));
        DashboardState::provide(
            use_signal(PrFilter::default),
            use_signal(|| Some("page-2".to_owned())),
            use_signal(|| Selection::of([grouped_row()])),
            use_signal(|| PageStatus::Loaded(loaded_page())),
            use_signal(|| SummaryStatus::Loaded(loaded_summary())),
            use_signal(|| FIXTURE_NOW),
            use_callback(move |()| {
                let mut reloads = reloads;
                *reloads.write() += 1;
            }),
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

    fn reloads(dom: &VirtualDom) -> u32 {
        dom.in_runtime(|| {
            *consume_context_from_scope::<Signal<u32>>(ScopeId::APP)
                .expect("the fixture counts reloads")
                .read()
        })
    }

    /// The sync glyph spins from the moment a manual sync is asked for until
    /// the live refresh sees the sweep reach the pull requests and ends it.
    /// Reloading the rows is not what ends it — a retry after a failed
    /// read, or a repository the sweep wrote ahead of its pull requests,
    /// reloads them with the sweep still running — so a reload leaves the
    /// sync in flight; a sync Restate did not take is over at once.
    #[test]
    fn a_manual_sync_is_in_flight_until_it_is_ended_however_often_the_rows_reload() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            assert!(!state.syncing());
            state.begin_sync();
            assert!(state.syncing());
            state.reload();
            assert!(state.syncing(), "a reload alone does not end the sync");
            assert_eq!(reloads(&dom), 1);

            state.end_sync();
            assert!(!state.syncing());
            assert_eq!(reloads(&dom), 1, "ending a sync does not reload");
        });
    }

    #[test]
    fn the_clock_is_the_one_the_dashboard_was_given() {
        let (dom, state) = mount();

        dom.in_runtime(|| assert_eq!(state.now(), FIXTURE_NOW));
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

    /// A filter change drops the selection; paging within the filter keeps
    /// it, so rows picked across pages add up to one batch.
    #[test]
    fn changing_or_clearing_the_filter_drops_the_selection_and_paging_keeps_it() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.update_filter(|filter| filter.needs_attention = true);
            assert!(state.filter().needs_attention);
            assert_eq!(*state.cursor(), None);
            assert_eq!(state.selected_count(), 0);

            state.toggle_selected(grouped_row());
            state.load_next("page-2".to_owned());
            assert_eq!(*state.cursor(), Some("page-2".to_owned()));
            assert_eq!(
                state.selected_count(),
                1,
                "the next page keeps the selection"
            );
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
            state.toggle_selected(grouped_row());
            assert!(!state.is_selected("7#9"));
            state.toggle_selected(serde_row());
            assert!(state.is_selected("8#12"));
            assert_eq!(state.selected_count(), 1);
        });
    }

    /// Back and forward land on a filter and a cursor together: the cursor
    /// belongs to that filter's paging, so it is kept rather than reset. The
    /// selection goes with the filter: landing on another filter drops it,
    /// landing on another page of the same filter keeps it.
    #[test]
    fn navigating_sets_the_filter_and_cursor_together_and_drops_the_selection_with_the_filter() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.navigate(PrFilter::default(), Some("page-3".to_owned()));
            assert_eq!(*state.cursor(), Some("page-3".to_owned()));
            assert_eq!(
                state.selected_count(),
                1,
                "another page of the same filter keeps the selection"
            );

            let filter = PrFilter {
                check_statuses: vec![CheckStatus::Failure],
                ..PrFilter::default()
            };
            state.navigate(filter.clone(), Some("page-3".to_owned()));
            assert_eq!(*state.filter(), filter);
            assert_eq!(*state.cursor(), Some("page-3".to_owned()));
            assert_eq!(state.selected_count(), 0);

            state.toggle_selected(grouped_row());
            state.navigate(PrFilter::default(), None);
            assert_eq!(*state.filter(), PrFilter::default());
            assert_eq!(*state.cursor(), None);
            assert_eq!(state.selected_count(), 0);
        });
    }

    /// With some of the page selected, "visible" selects the rest; only once
    /// every row is selected does it deselect them all. A row off the page
    /// is neither counted as visible nor touched.
    #[test]
    fn toggling_the_visible_rows_selects_them_all_before_it_clears_them() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.toggle_selected(off_page_row());
            assert_eq!(state.visible_selected_count(), 1);
            state.toggle_visible();
            assert_eq!(state.visible_selected_count(), 2);
            assert_eq!(state.selected_count(), 3);
            state.toggle_visible();
            assert_eq!(state.visible_selected_count(), 0);
            assert_eq!(
                state.selected_count(),
                1,
                "the row off the page stays selected"
            );
        });
    }

    /// The rows a bulk action will target, newest update first as the table
    /// lists them. A row the page still shows is taken from the page, so the
    /// head SHA the confirmation names is the one on screen; a row the page
    /// no longer shows is remembered as it was when picked.
    #[test]
    fn the_selected_rows_are_kept_off_the_page_and_refreshed_from_it_when_on() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.toggle_selected(off_page_row());
            let mut moved_on = serde_row();
            moved_on.head_sha = "before".to_owned();
            state.toggle_selected(moved_on);

            let rows = state.selected_rows();
            let numbers: Vec<u64> = rows.iter().map(|row| row.number).collect();
            assert_eq!(numbers, vec![9, 12, 13]);
            assert_eq!(
                rows[1].head_sha, "def456",
                "the page's copy of a row it shows wins over the remembered one"
            );
            assert_eq!(rows[2].head_sha, "0ff9a6e");
        });
    }

    /// "Select all matching" fills the selection from the read model's
    /// answer, rows on the page or not, in place of whatever was picked
    /// before; when the batch limit left some of the filter's rows out, it
    /// remembers how many matched in all so the bar can say so. The note is
    /// about how the selection was made, so editing it by hand, or dropping
    /// it, forgets the note.
    #[test]
    fn selecting_all_matching_replaces_the_selection_and_notes_when_the_limit_cut_it_short() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            assert!(state.is_selected("7#9"));
            state.select_matching(
                PrFilter::default(),
                DashboardPage {
                    rows: vec![serde_row(), off_page_row()],
                    total: 52,
                    next_cursor: Some("more".to_owned()),
                },
            );
            assert!(!state.is_selected("7#9"), "the old selection is replaced");
            assert!(state.is_selected("8#12"));
            assert!(state.is_selected("8#13"));
            assert_eq!(state.selected_count(), 2);
            assert_eq!(state.capped_from(), Some(52));

            state.toggle_selected(off_page_row());
            assert_eq!(
                state.capped_from(),
                None,
                "a selection edited by hand is no longer the first so many matching"
            );

            state.select_matching(
                PrFilter::default(),
                DashboardPage {
                    rows: vec![serde_row()],
                    total: 1,
                    next_cursor: None,
                },
            );
            assert_eq!(state.selected_count(), 1);
            assert_eq!(
                state.capped_from(),
                None,
                "every matching row fit, so nothing was cut short"
            );
        });
    }

    /// A bulk action's rows leave the selection when it is queued, and only
    /// they do: one pull request merged from its drawer leaves the rest of
    /// the selection standing. A row that was never selected is nothing to
    /// take out. Like any other edit, it forgets that the selection was the
    /// first so many matching.
    #[test]
    fn deselecting_takes_out_only_the_rows_named_and_forgets_the_cap_note() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            state.toggle_selected(serde_row());
            state.toggle_selected(off_page_row());
            assert_eq!(state.selected_count(), 3);

            state.deselect(&[grouped_row()]);
            assert!(!state.is_selected("7#9"));
            assert!(state.is_selected("8#12"));
            assert!(state.is_selected("8#13"));
            assert_eq!(state.selected_count(), 2);

            state.deselect(&[grouped_row()]);
            assert_eq!(state.selected_count(), 2, "nothing to take out");

            state.select_matching(
                PrFilter::default(),
                DashboardPage {
                    rows: vec![serde_row(), off_page_row()],
                    total: 52,
                    next_cursor: Some("more".to_owned()),
                },
            );
            assert_eq!(state.capped_from(), Some(52));
            state.deselect(&[serde_row()]);
            assert_eq!(state.selected_count(), 1);
            assert_eq!(
                state.capped_from(),
                None,
                "a selection with a row taken out is no longer the first so many matching"
            );

            state.deselect(&[off_page_row()]);
            assert_eq!(state.selected_count(), 0);
        });
    }

    /// The read model answers "select all matching" after a round trip, by
    /// which time the user may have changed the filter, and with it dropped
    /// the selection. Rows matched for a filter no longer in force would put
    /// a selection back that does not belong to what is on screen, so they
    /// are ignored.
    #[test]
    fn a_selection_matched_for_a_filter_no_longer_in_force_is_ignored() {
        let (dom, mut state) = mount();

        dom.in_runtime(|| {
            let asked_with = PrFilter::default();
            state.update_filter(|filter| filter.needs_attention = true);
            assert_eq!(state.selected_count(), 0);

            state.select_matching(
                asked_with,
                DashboardPage {
                    rows: vec![serde_row()],
                    total: 1,
                    next_cursor: None,
                },
            );
            assert_eq!(
                state.selected_count(),
                0,
                "the answer was for another filter"
            );
            assert_eq!(state.capped_from(), None);
        });
    }
}
