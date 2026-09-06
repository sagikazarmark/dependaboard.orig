//! How the URL and the dashboard keep in step. The dashboard opens on the
//! state its URL names; from then on the URL follows the filter, cursor, open
//! pull request, and followed batch, and the dashboard follows the URL when
//! the browser moves through its history.
//!
//! The batch is the one part that rides along rather than being a move of its
//! own: the pill is over every page, so following a batch changes the address
//! in place, and back and forward keep following it. The URL's word on the
//! batch is taken once, when the dashboard opens following nothing, which is
//! what carries a batch across a reload.

use std::sync::Arc;

use dioxus::history::history;
use dioxus::prelude::*;

use crate::ui::batch::Followed;
use crate::ui::dashboard_state::DashboardState;
use crate::ui::detail_drawer::OpenPr;
use crate::ui::url_state::UrlState;

/// The state the URL named when the dashboard opened. Read once, so the
/// signals can start from it and the first render asks the read model for
/// the linked page rather than the default one.
pub(crate) fn use_url_state() -> UrlState {
    use_hook(|| UrlState::from_route(&history().current_route()))
}

/// Keeps the URL and the dashboard in step from the first render on.
///
/// The browser is told about moves the dashboard makes; the dashboard is told
/// about moves the browser makes on its own, which is back and forward. The
/// two never chase each other: each side acts only when the other has moved
/// to a state it is not already in. `followed` is the batch the dashboard
/// follows, which the URL carries; `onbatch` is asked to follow the batch the
/// URL names while the dashboard follows none.
pub(crate) fn use_url_sync(
    mut state: DashboardState,
    mut detail: Signal<Option<OpenPr>>,
    followed: Signal<Option<Followed>>,
    onbatch: Callback<String>,
) {
    let history = use_hook(history);
    // The followed batch by id alone, so the URL is not rewritten for every
    // poll that moves the batch along.
    let batch_id = use_memo(move || {
        followed
            .read()
            .as_ref()
            .map(|followed| followed.batch_id.clone())
    });

    // How many times the browser has moved on its own. Bumped by the
    // `popstate` listener, whose callback has to be `Send + Sync`; a plain
    // signal is not, so this one is the kind that is.
    let moved = use_signal_sync(|| 0u32);
    use_hook({
        let history = history.clone();
        move || {
            history.updater(Arc::new(move || {
                let mut moved = moved;
                *moved.write() += 1;
            }));
        }
    });

    // The dashboard follows the browser. The batch is the exception: an
    // address from before the batch, or from before it was replaced or given
    // up, still names what was followed then, and back should not put the
    // batch down or take an old one up again. The URL's batch is followed
    // once, when the dashboard opens, which is the reload it is there for.
    let mut opened = use_hook(|| CopyValue::new(false));
    use_effect({
        let history = history.clone();
        move || {
            moved.read();
            let next = UrlState::from_route(&history.current_route());
            state.navigate(next.filter, next.cursor);
            let open = detail.peek().as_ref().map(OpenPr::key);
            if open != next.pr {
                detail.set(next.pr.map(OpenPr::Loading));
            }
            if let Some(batch) = next.batch
                && !opened.replace(true)
            {
                onbatch.call(batch);
            }
        }
    });

    // The browser follows the dashboard. Whether the open pull request was
    // still being read the last time round is remembered, because a drawer
    // that never opened leaves no entry to go back to.
    let mut was_loading = use_hook(|| CopyValue::new(false));
    use_effect(move || {
        let open = detail.read();
        let loading = matches!(&*open, Some(OpenPr::Loading(_)));
        let next = UrlState {
            filter: state.filter().clone(),
            cursor: state.cursor().clone(),
            pr: open.as_ref().map(OpenPr::key),
            batch: batch_id.read().clone(),
        };
        drop(open);
        let gave_up = was_loading.replace(loading) && next.pr.is_none();
        let current = history.current_route();
        let previous = UrlState::from_route(&current);
        let route = next.to_route();
        if next == previous {
            // The browser is already here. A link that carried values the
            // dashboard dropped, or in another order, is tidied in place.
            if route != current {
                history.replace(route);
            }
        } else if gave_up || next.refines_search_of(&previous) || next.same_view_as(&previous) {
            // The linked pull request could not be read, the search text was
            // refined, or only the followed batch differs: an entry back would
            // fail the same way, step through what was typed, or put a running
            // batch down, so this state takes the last one's place.
            history.replace(route);
        } else {
            history.push(route);
        }
    });
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use dependaboard_core::{CheckStatus, PageCursor, PrFilter, PrKey, UpdateType};
    use dioxus::core::consume_context_from_scope;
    use dioxus::history::{History, MemoryHistory};

    use super::*;
    use crate::ui::batch::Followed;
    use crate::ui::dashboard_state::{
        Answers, CapabilitiesStatus, PageStatus, Selection, SummaryStatus,
    };
    use crate::ui::test_support::{
        BATCH, FIXTURE_NOW, OTHER_BATCH, grouped_row, loaded_page, loaded_summary,
    };

    /// The batch ids the URL has told the dashboard to follow, in order.
    #[derive(Clone, Copy)]
    struct Adopted(Signal<Vec<String>>);

    /// A browser history: in memory, and firing `popstate` — the callback
    /// given to [`History::updater`] — when it goes back or forward, as the
    /// browser does and [`MemoryHistory`] alone does not.
    struct TestHistory {
        memory: MemoryHistory,
        on_pop: RefCell<Option<Arc<dyn Fn() + Send + Sync>>>,
    }

    impl TestHistory {
        fn at(route: &str) -> Rc<Self> {
            Rc::new(Self {
                memory: MemoryHistory::with_initial_path(route),
                on_pop: RefCell::new(None),
            })
        }

        fn fire_popstate(&self) {
            if let Some(on_pop) = &*self.on_pop.borrow() {
                on_pop();
            }
        }
    }

    impl History for TestHistory {
        fn current_route(&self) -> String {
            self.memory.current_route()
        }

        fn can_go_back(&self) -> bool {
            self.memory.can_go_back()
        }

        fn go_back(&self) {
            self.memory.go_back();
            self.fire_popstate();
        }

        fn can_go_forward(&self) -> bool {
            self.memory.can_go_forward()
        }

        fn go_forward(&self) {
            self.memory.go_forward();
            self.fire_popstate();
        }

        fn push(&self, route: String) {
            self.memory.push(route);
        }

        fn replace(&self, route: String) {
            self.memory.replace(route);
        }

        fn updater(&self, callback: Arc<dyn Fn() + Send + Sync>) {
            *self.on_pop.borrow_mut() = Some(callback);
        }
    }

    /// The dashboard's signals as [`Dashboard`] wires them, without the read
    /// model: seeded from the URL, then kept in step with it.
    ///
    /// [`Dashboard`]: crate::ui::dashboard::Dashboard
    fn Fixture() -> Element {
        let UrlState {
            filter, cursor, pr, ..
        } = use_url_state();
        let detail = use_context_provider(|| Signal::new(pr.map(OpenPr::Loading)));
        let mut followed = use_context_provider(|| Signal::new(None::<Followed>));
        let mut adopted = use_context_provider(|| Adopted(Signal::new(Vec::new())));
        // As the dashboard does: a batch the URL names is followed by its id.
        let onbatch = use_callback(move |batch_id: String| {
            adopted.0.write().push(batch_id.clone());
            followed.set(Some(Followed::attaching(&batch_id, FIXTURE_NOW)));
        });
        let state = DashboardState::provide(
            use_signal(|| filter),
            use_signal(|| cursor),
            use_signal(Selection::default),
            Answers {
                page: use_signal(|| PageStatus::Loaded(loaded_page())).into(),
                summary: use_signal(|| SummaryStatus::Loaded(loaded_summary())).into(),
                capabilities: use_signal(|| CapabilitiesStatus::Loading).into(),
            },
            use_signal(|| FIXTURE_NOW),
            use_callback(|_| {}),
        );
        use_url_sync(state, detail, followed, onbatch);
        rsx! {}
    }

    struct Mounted {
        dom: VirtualDom,
        history: Rc<TestHistory>,
        state: DashboardState,
        detail: Signal<Option<OpenPr>>,
        followed: Signal<Option<Followed>>,
        adopted: Adopted,
    }

    impl Mounted {
        fn at(route: &str) -> Self {
            let history = TestHistory::at(route);
            let mut dom =
                VirtualDom::new(Fixture).with_root_context(history.clone() as Rc<dyn History>);
            dom.rebuild_in_place();
            let (state, detail, followed, adopted) = dom.in_runtime(|| {
                (
                    consume_context_from_scope::<DashboardState>(ScopeId::APP),
                    consume_context_from_scope::<Signal<Option<OpenPr>>>(ScopeId::APP),
                    consume_context_from_scope::<Signal<Option<Followed>>>(ScopeId::APP),
                    consume_context_from_scope::<Adopted>(ScopeId::APP),
                )
            });
            let mut mounted = Self {
                dom,
                history,
                state: state.expect("the fixture provides the dashboard state"),
                detail: detail.expect("the fixture provides the open pull request"),
                followed: followed.expect("the fixture provides the followed batch"),
                adopted: adopted.expect("the fixture provides the adopted batches"),
            };
            mounted.settle();
            mounted
        }

        /// Makes `change` on the dashboard, then lets the effects it set off
        /// run to completion, as the browser would before the next event.
        fn act(&mut self, change: impl FnOnce(&mut DashboardState, &mut Signal<Option<OpenPr>>)) {
            let mut state = self.state;
            let mut detail = self.detail;
            self.dom.in_runtime(|| change(&mut state, &mut detail));
            self.settle();
        }

        /// Makes the dashboard follow `batch`, or none, as queuing a batch or
        /// giving one up does, and lets the effects run.
        fn follow(&mut self, batch: Option<&str>) {
            let mut followed = self.followed;
            self.dom.in_runtime(|| {
                followed.set(batch.map(|batch| Followed::attaching(batch, FIXTURE_NOW)));
            });
            self.settle();
        }

        fn followed_batch(&self) -> Option<String> {
            self.dom.in_runtime(|| {
                self.followed
                    .read()
                    .as_ref()
                    .map(|followed| followed.batch_id.clone())
            })
        }

        fn adopted(&self) -> Vec<String> {
            self.dom.in_runtime(|| self.adopted.0.read().clone())
        }

        /// Lets pending tasks and effects run. An effect that writes a signal
        /// queues the next round, so a few rounds cover the cascade.
        fn settle(&mut self) {
            for _ in 0..4 {
                self.dom.render_immediate_to_vec();
            }
        }

        fn route(&self) -> String {
            self.history.current_route()
        }

        fn filter(&self) -> PrFilter {
            self.dom.in_runtime(|| self.state.filter().clone())
        }

        fn cursor(&self) -> Option<String> {
            self.dom.in_runtime(|| self.state.cursor().clone())
        }

        fn open_pr(&self) -> Option<OpenPr> {
            self.dom.in_runtime(|| self.detail.read().clone())
        }
    }

    fn cursor() -> String {
        PageCursor {
            updated_at: 1,
            id: "7#9".to_owned(),
        }
        .encode()
    }

    #[test]
    fn the_dashboard_opens_on_the_state_its_url_names() {
        let mounted = Mounted::at(&format!(
            "/?view=attention&type=major&after={}&pr=7%239",
            cursor()
        ));

        assert_eq!(
            mounted.filter(),
            PrFilter {
                update_types: vec![UpdateType::Major],
                needs_attention: true,
                ..PrFilter::default()
            }
        );
        assert_eq!(mounted.cursor(), Some(cursor()));
        assert_eq!(mounted.open_pr(), Some(OpenPr::Loading(PrKey::new(7, 9))));
        assert!(!mounted.history.can_go_back());
    }

    /// A link may carry values the dashboard drops, or list them in another
    /// order. The dashboard opens on what is left, and tidies the address bar
    /// to say the same, without an entry to go back to.
    #[test]
    fn a_link_with_stray_values_is_tidied_in_place() {
        let mounted = Mounted::at("/?utm_source=slack&type=huge&check=failure&type=major");

        assert_eq!(
            mounted.filter(),
            PrFilter {
                update_types: vec![UpdateType::Major],
                check_statuses: vec![CheckStatus::Failure],
                ..PrFilter::default()
            }
        );
        assert_eq!(mounted.route(), "/?type=major&check=failure");
        assert!(!mounted.history.can_go_back());
    }

    /// Each move the dashboard makes — a facet, the next page, a drawer —
    /// takes the browser to the matching address, with the last one behind it.
    #[test]
    fn the_dashboards_moves_reach_the_address_bar() {
        let mut mounted = Mounted::at("/");

        mounted.act(|state, _| {
            state.toggle_filter(|filter| &mut filter.check_statuses, CheckStatus::Failure)
        });
        assert_eq!(mounted.route(), "/?check=failure");
        assert!(mounted.history.can_go_back());

        mounted.act(|state, _| state.load_next(cursor()));
        assert_eq!(
            mounted.route(),
            format!("/?check=failure&after={}", cursor())
        );

        mounted.act(|_, detail| detail.set(Some(OpenPr::from(grouped_row()))));
        assert_eq!(
            mounted.route(),
            format!("/?check=failure&after={}&pr=7%239", cursor())
        );

        mounted.act(|_, detail| detail.set(None));
        assert_eq!(
            mounted.route(),
            format!("/?check=failure&after={}", cursor())
        );
    }

    /// Back and forward move the dashboard through the states it has been
    /// in: the filter, the page, and the drawer follow the address bar, and
    /// the browser is not told about a move it made itself. The selection
    /// goes with the filter: back a page keeps it, back a filter drops it.
    #[test]
    fn back_and_forward_move_the_dashboard_through_its_history() {
        let mut mounted = Mounted::at("/");
        mounted.act(|state, _| {
            state.toggle_filter(|filter| &mut filter.check_statuses, CheckStatus::Failure)
        });
        mounted.act(|state, _| state.load_next(cursor()));
        mounted.act(|state, detail| {
            state.toggle_selected(grouped_row());
            detail.set(Some(OpenPr::from(grouped_row())));
        });

        mounted.history.go_back();
        mounted.settle();
        assert_eq!(mounted.open_pr(), None);
        assert_eq!(mounted.cursor(), Some(cursor()));
        assert_eq!(
            mounted.route(),
            format!("/?check=failure&after={}", cursor())
        );
        assert!(mounted.history.can_go_forward());

        mounted.history.go_back();
        mounted.settle();
        assert_eq!(mounted.cursor(), None);
        assert_eq!(mounted.filter().check_statuses, vec![CheckStatus::Failure]);
        assert_eq!(mounted.dom.in_runtime(|| mounted.state.selected_count()), 1);

        mounted.history.go_back();
        mounted.settle();
        assert_eq!(mounted.filter(), PrFilter::default());
        assert_eq!(mounted.route(), "/");
        assert!(!mounted.history.can_go_back());
        assert_eq!(mounted.dom.in_runtime(|| mounted.state.selected_count()), 0);

        mounted.history.go_forward();
        mounted.settle();
        assert_eq!(mounted.filter().check_statuses, vec![CheckStatus::Failure]);
        assert_eq!(mounted.route(), "/?check=failure");
        assert!(mounted.history.can_go_forward());
    }

    /// Going back to an address that names a pull request opens its drawer
    /// again, by key: the row is read afresh.
    #[test]
    fn going_back_to_an_open_drawer_asks_for_its_pull_request_again() {
        let mut mounted = Mounted::at("/");
        mounted.act(|_, detail| detail.set(Some(OpenPr::from(grouped_row()))));
        mounted.act(|_, detail| detail.set(None));

        mounted.history.go_back();
        mounted.settle();

        assert_eq!(mounted.open_pr(), Some(OpenPr::Loading(PrKey::new(7, 9))));
        assert_eq!(mounted.route(), "/?pr=7%239");
    }

    /// Typing reaches the filter a debounced word at a time. Back from a
    /// search returns to before it, not to what was typed along the way.
    #[test]
    fn back_from_a_search_returns_to_before_it() {
        let mut mounted = Mounted::at("/");
        mounted.act(|state, _| state.update_filter(|filter| filter.query = Some("ser".to_owned())));
        mounted
            .act(|state, _| state.update_filter(|filter| filter.query = Some("serde".to_owned())));
        assert_eq!(mounted.route(), "/?q=serde");

        mounted.history.go_back();
        mounted.settle();

        assert_eq!(mounted.route(), "/");
        assert_eq!(mounted.filter().query, None);
    }

    /// A link may name a pull request that has since gone, or cannot be read.
    /// The drawer gives it up before it ever opened, and the address is set
    /// right in place: an entry back would only fail the same way again.
    #[test]
    fn a_link_to_a_pull_request_that_cannot_be_read_leaves_no_entry_behind() {
        let mut mounted = Mounted::at("/?check=failure&pr=7%239");

        mounted.act(|_, detail| detail.set(None));

        assert_eq!(mounted.route(), "/?check=failure");
        assert!(!mounted.history.can_go_back());
    }
    /// A reload keeps the batch: the dashboard opens following the batch its
    /// URL names, by id, before Restate has said a word about it.
    #[test]
    fn the_dashboard_opens_following_the_batch_its_url_names() {
        let mounted = Mounted::at(&format!("/?check=failure&batch={BATCH}"));

        assert_eq!(mounted.adopted(), [BATCH]);
        assert_eq!(mounted.followed_batch(), Some(BATCH.to_owned()));
        assert_eq!(mounted.filter().check_statuses, vec![CheckStatus::Failure]);
        assert_eq!(mounted.route(), format!("/?check=failure&batch={BATCH}"));
        assert!(!mounted.history.can_go_back());
    }

    /// Queuing a batch puts it in the address bar, so a reload finds it, but
    /// leaves no entry behind: the pill is over the page, not a page of its
    /// own, and back should not put a running batch down.
    #[test]
    fn following_a_batch_reaches_the_address_bar_without_an_entry() {
        let mut mounted = Mounted::at("/?check=failure");

        mounted.follow(Some(BATCH));

        assert_eq!(mounted.route(), format!("/?check=failure&batch={BATCH}"));
        assert!(!mounted.history.can_go_back());
        assert!(
            mounted.adopted().is_empty(),
            "the dashboard's own batch is not told back to it"
        );
    }

    /// The batch a dashboard follows rides along through its history: back
    /// to an address from before the batch keeps following it, and the
    /// address is set right to say so.
    #[test]
    fn back_keeps_following_the_batch() {
        let mut mounted = Mounted::at("/");
        mounted.act(|state, _| {
            state.toggle_filter(|filter| &mut filter.check_statuses, CheckStatus::Failure)
        });
        mounted.follow(Some(BATCH));

        mounted.history.go_back();
        mounted.settle();

        assert_eq!(mounted.filter(), PrFilter::default());
        assert_eq!(mounted.followed_batch(), Some(BATCH.to_owned()));
        assert_eq!(mounted.route(), format!("/?batch={BATCH}"));
        assert!(mounted.adopted().is_empty());
    }

    /// A batch another batch has replaced, or one Restate turned out not to
    /// have, is not followed back into. An address from before names it
    /// still; the dashboard keeps to the batch it follows now, and the
    /// address follows the dashboard.
    #[test]
    fn a_batch_no_longer_followed_is_not_taken_up_again_from_an_old_address() {
        let mut mounted = Mounted::at(&format!("/?batch={BATCH}"));
        mounted.act(|state, _| {
            state.toggle_filter(|filter| &mut filter.check_statuses, CheckStatus::Failure)
        });
        mounted.follow(Some(OTHER_BATCH));

        mounted.history.go_back();
        mounted.settle();

        assert_eq!(mounted.followed_batch(), Some(OTHER_BATCH.to_owned()));
        assert_eq!(mounted.route(), format!("/?batch={OTHER_BATCH}"));
        assert_eq!(
            mounted.adopted(),
            [BATCH],
            "adopted once, when the page opened"
        );
    }

    /// A batch given up — Restate had no progress for it — leaves the address
    /// bar in place: an entry back would only find nothing again.
    #[test]
    fn giving_a_batch_up_leaves_no_entry_behind() {
        let mut mounted = Mounted::at(&format!("/?check=failure&batch={BATCH}"));

        mounted.follow(None);

        assert_eq!(mounted.route(), "/?check=failure");
        assert!(!mounted.history.can_go_back());
    }

    /// An address from before a batch was given up still names it. Back to
    /// it does not ask after the batch again — Restate said what it had — and
    /// the address is set right.
    #[test]
    fn a_batch_given_up_is_not_taken_up_again_from_an_old_address() {
        let mut mounted = Mounted::at(&format!("/?batch={BATCH}"));
        mounted.act(|state, _| {
            state.toggle_filter(|filter| &mut filter.check_statuses, CheckStatus::Failure)
        });
        mounted.follow(None);

        mounted.history.go_back();
        mounted.settle();

        assert_eq!(mounted.followed_batch(), None);
        assert_eq!(mounted.route(), "/");
        assert_eq!(
            mounted.adopted(),
            [BATCH],
            "adopted once, when the page opened"
        );
    }
}
