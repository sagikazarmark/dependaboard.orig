//! The drawer for one pull request: its projection, durable state, and the
//! per-row rebase, merge, and sync actions. A pull request a link names by
//! key alone has its row read here before the drawer opens. The drawer
//! follows the page from then on: a reload that changes its row reaches it,
//! and a pull request the read model no longer has is said to be no longer
//! open, with its actions withheld.

use dependaboard_core::{BulkActionKind, DashboardPage, PrKey, PrRecord, PrState};
use dioxus::logger::tracing;
use dioxus::prelude::*;

use crate::api::{load_pr_projection, load_pr_status, request_pr_sync};
use crate::components::button::{Button, ButtonSize};
use crate::components::loading::{Loading, LoadingSize};
use crate::ui::dashboard_state::use_dashboard;
use crate::ui::format::{relative_time, status_class, status_label, update_class, version_label};
use crate::ui::pr_sync::wait_for_pr_sync_completion;
use crate::ui::side_panel::SidePanel;
use crate::ui::{PendingAction, user_facing};

/// The pull request whose drawer is open. A row the user clicked is in hand
/// at once; a link names the pull request by key alone, and the drawer waits
/// for its row to be read. A pull request that leaves the read model while
/// its drawer is open — merged, closed, or its repository gone — is kept as
/// the drawer last saw it, so the drawer can say which pull request it was
/// and that it is no longer open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OpenPr {
    Loading(PrKey),
    Loaded(Box<PrRecord>),
    Gone(Box<PrRecord>),
}

impl OpenPr {
    pub(crate) fn key(&self) -> PrKey {
        match self {
            Self::Loading(key) => key.clone(),
            Self::Loaded(row) | Self::Gone(row) => row.key(),
        }
    }

    /// The row the drawer shows, once it has one: the pull request as last
    /// read, whether it is still open or not.
    fn row(&self) -> Option<&PrRecord> {
        match self {
            Self::Loading(_) => None,
            Self::Loaded(row) | Self::Gone(row) => Some(row),
        }
    }
}

impl From<PrRecord> for OpenPr {
    fn from(row: PrRecord) -> Self {
        Self::Loaded(Box::new(row))
    }
}

/// The drawer for the pull request in `detail`, if one is open. The drawer is
/// keyed by the pull request, so opening another one mounts a fresh drawer,
/// which reads that pull request's durable state; the key is honoured because
/// the drawer sits in an `if` body, which Dioxus diffs as a keyed list.
///
/// A pull request still [`OpenPr::Loading`] has its row read here. The drawer
/// opens once the row is in hand; if the pull request has gone since the link
/// was made, or cannot be read, nothing opens. `now` is the dashboard's
/// clock, which the drawer's times are read against.
///
/// The drawer follows the page: each time the page in force lands, the open
/// pull request's row is taken from it if the page shows the row, so a live
/// refresh or a finished batch reaches the drawer as it does the table. A
/// row the page does not show is asked about, since the page is one slice of
/// one filter; a pull request the read model no longer has is
/// [`OpenPr::Gone`] from then on, until a page shows it again.
#[component]
pub(crate) fn OpenDetail(
    mut detail: Signal<Option<OpenPr>>,
    now: u64,
    onaction: EventHandler<PendingAction>,
    onsync: EventHandler<Result<Option<PrRecord>, String>>,
) -> Element {
    let state = use_dashboard();
    use_effect(move || {
        let Some(OpenPr::Loading(key)) = &*detail.read() else {
            return;
        };
        let key = key.clone();
        spawn(async move {
            let row = load_pr_projection(key.repository_id, key.number).await;
            // The user may have opened another pull request, or moved on,
            // while the row was in flight; then it is not theirs to see.
            if *detail.peek() != Some(OpenPr::Loading(key.clone())) {
                return;
            }
            detail.set(match row {
                Ok(row) => row.map(OpenPr::from),
                Err(error) => {
                    tracing::warn!(%error, %key, "the linked pull request could not be read");
                    None
                }
            });
        });
    });
    // Runs when the page lands, not when the drawer changes: the open pull
    // request is peeked at, so replacing its row here does not run this
    // again.
    use_effect(move || {
        let followed = {
            let page = state.page.read();
            let Some(page) = page.loaded() else {
                return;
            };
            let open = detail.peek();
            let Some(open) = &*open else {
                return;
            };
            follow(open, page)
        };
        match followed {
            Followed::AsIs => {}
            Followed::Replaced(row) => detail.set(Some(OpenPr::Loaded(row))),
            Followed::Unknown(last) => {
                let asked = OpenPr::Loaded(last.clone());
                spawn(async move {
                    let key = last.key();
                    let answer = load_pr_projection(key.repository_id, key.number).await;
                    // The answer is for the drawer as it was when it asked. The
                    // user may have opened another pull request meanwhile, or
                    // closed it; or a later page may have replaced the row
                    // already, and an answer read before that is the older one.
                    if detail.peek().as_ref() != Some(&asked) {
                        return;
                    }
                    match answer {
                        Ok(Some(row)) if row != *last => detail.set(Some(OpenPr::from(row))),
                        Ok(Some(_)) => {}
                        Ok(None) => detail.set(Some(OpenPr::Gone(last))),
                        Err(error) => {
                            tracing::warn!(%error, %key, "the open pull request could not be read again");
                        }
                    }
                });
            }
        }
    });
    let open = detail.read();
    rsx! {
        if let Some(row) = open.as_ref().and_then(OpenPr::row) {
            DetailDrawer {
                key: "{row.id}",
                row: row.clone(),
                gone: matches!(&*open, Some(OpenPr::Gone(_))),
                rebase_withheld: state.rebase_withheld(),
                now,
                onclose: move |_| detail.set(None),
                onaction,
                onsync,
            }
        }
    }
}

/// What a page that has just landed says about the open pull request.
#[derive(Clone, Debug, PartialEq)]
enum Followed {
    /// Nothing to do: the page shows the row as the drawer has it; or there
    /// is no row yet to follow; or the drawer has given the pull request up
    /// for gone and the page does not show it, which is no news.
    AsIs,
    /// The page shows the row otherwise than the drawer has it — a moved
    /// head, a rollup that settled, a row back after the drawer had given it
    /// up for gone — and this is the row to show.
    Replaced(Box<PrRecord>),
    /// The drawer shows this row as open and the page does not show it. That
    /// says nothing either way: the page is one slice of one filter, and the
    /// pull request may be on another page, outside the filter, or gone. It
    /// has to be asked about; this is the row to fall back on if it is gone.
    Unknown(Box<PrRecord>),
}

/// Reads `page` for the row of `open`, as [`OpenDetail`] does each time the
/// page in force lands.
fn follow(open: &OpenPr, page: &DashboardPage) -> Followed {
    let Some(shown) = open.row() else {
        return Followed::AsIs;
    };
    match (page.rows.iter().find(|row| row.id == shown.id), open) {
        (Some(row), OpenPr::Loaded(_)) if row == shown => Followed::AsIs,
        (Some(row), _) => Followed::Replaced(Box::new(row.clone())),
        (None, OpenPr::Loaded(row)) => Followed::Unknown(row.clone()),
        (None, _) => Followed::AsIs,
    }
}

/// The drawer for `row`. `gone` says the pull request has left the read
/// model since the drawer opened: the drawer still shows the row, as the
/// last the dashboard saw of it, but says so and withholds every action. The
/// bulk actions would be rejected against a head that is gone, and a sync
/// starts from the row in the read model, which is no longer there.
/// `rebase_withheld` is why a rebase alone is not on offer, if the service
/// has said it is not; the rest of the actions run as the App and stand.
#[component]
pub(crate) fn DetailDrawer(
    row: PrRecord,
    #[props(default)] gone: bool,
    #[props(default)] rebase_withheld: Option<&'static str>,
    now: u64,
    onclose: EventHandler<()>,
    onaction: EventHandler<PendingAction>,
    onsync: EventHandler<Result<Option<PrRecord>, String>>,
) -> Element {
    let stale = row.is_stale(now);
    let mut syncing = use_signal(|| false);
    let mut sync_queued = use_signal(|| false);
    let sync_repository_id = row.repository_id;
    let sync_number = row.number;
    // Read when the drawer mounts, and again whenever the row it shows is
    // replaced or found gone: both mean something happened to the pull
    // request that Restate may have recorded. The pull request itself cannot
    // change under one drawer, which is keyed by it.
    let status = use_resource(use_reactive(
        (&row.synced_at, &gone),
        move |(_synced_at, _gone)| load_pr_status(sync_repository_id, sync_number),
    ));
    let durable_state = DurableStatus::from_resource(status.read().as_ref());
    let subject = row.clone();
    let request = use_callback(move |action: BulkActionKind| {
        onaction.call(PendingAction {
            action,
            rows: vec![subject.clone()],
            retried_from: None,
        });
    });
    rsx! {
        SidePanel {
            class: "detail-drawer",
            eyebrow: "Pull request",
            onclose,
            title: rsx! {
                a {
                    class: "github-pr-link",
                    href: row.html_url.clone(),
                    target: "_blank",
                    rel: "noreferrer",
                    "{row.owner}/{row.repo}#{row.number}"
                    span { class: "external-link-glyph", "↗" }
                }
            },
            div { class: "drawer-body",
                h3 { "{row.title}" }
                if gone {
                    p { class: "drawer-gone",
                        "This pull request is no longer open. It has left the read model since the drawer opened; what is shown is the last the dashboard saw of it."
                    }
                }
                div { class: "drawer-badges",
                    span { class: "update-chip {update_class(row.update_type)}", "{row.update_type}" }
                    span { class: "status-badge", span { class: "check-dot {status_class(row.check_status)}" } "{status_label(row.check_status)}" }
                    span { class: "status-badge", "{row.mergeable}" }
                    if stale { span { class: "status-badge stale-badge", "projection stale" } }
                }
                div { class: "drawer-actions",
                    Button {
                        size: ButtonSize::Sm,
                        class: "rebase-button",
                        disabled: gone || rebase_withheld.is_some(),
                        onclick: move |_| request(BulkActionKind::Rebase),
                        "Rebase"
                    }
                    Button {
                        size: ButtonSize::Sm,
                        class: "update-branch-button",
                        disabled: gone,
                        onclick: move |_| request(BulkActionKind::UpdateBranch),
                        "Update branch"
                    }
                    Button {
                        size: ButtonSize::Sm,
                        class: "merge-button",
                        disabled: gone,
                        onclick: move |_| request(BulkActionKind::Merge),
                        "Merge"
                    }
                    Button {
                        size: ButtonSize::Sm,
                        class: "drawer-sync",
                        disabled: gone || syncing() || sync_queued(),
                        onclick: move |_| {
                            syncing.set(true);
                            spawn(async move {
                                match request_pr_sync(sync_repository_id, sync_number).await {
                                    Ok(completion_id) => {
                                        syncing.set(false);
                                        sync_queued.set(true);
                                        match wait_for_pr_sync_completion(
                                            sync_repository_id,
                                            sync_number,
                                            completion_id,
                                        ).await {
                                            Ok(row) => {
                                                sync_queued.set(false);
                                                // The row the sync returns replaces the
                                                // drawer's, which re-reads the durable state.
                                                onsync.call(Ok(row));
                                            }
                                            Err(error) => {
                                                sync_queued.set(false);
                                                onsync.call(Err(format!(
                                                    "Sync was queued, but completion could not be confirmed: {error}"
                                                )));
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        syncing.set(false);
                                        onsync.call(Err(format!(
                                            "Could not queue sync: {}",
                                            user_facing(&error)
                                        )));
                                    }
                                }
                            });
                        },
                        if syncing() {
                            "Queueing..."
                        } else if sync_queued() {
                            "Syncing..."
                        } else {
                            "Sync"
                        }
                    }
                }
                if let Some(reason) = rebase_withheld {
                    p { class: "drawer-note", "{reason}" }
                }
                dl { class: "detail-list",
                    dt { "Head SHA" } dd { code { "{row.head_sha}" } }
                    dt { "Last updated" } dd { "{relative_time(now, row.updated_at)}" }
                    dt { "Projected" } dd { "{relative_time(now, row.synced_at)}" }
                }
                h4 { "Dependencies" }
                div { class: "dependency-list",
                    for dependency in &row.dependencies {
                        div {
                            strong { "{dependency.name}" }
                            code { "{version_label(dependency.from_version.as_deref(), dependency.to_version.as_deref())}" }
                            span { class: "update-chip {update_class(dependency.update_type)}", "{dependency.update_type}" }
                        }
                    }
                }
                h4 { "Labels" }
                div { class: "drawer-labels",
                    for label in &row.labels { span { "{label}" } }
                }
                h4 { "Durable state" }
                DurableState { status: durable_state, now }
            }
        }
    }
}

/// What the drawer knows about the pull request's durable state, read off the
/// `PullRequest/status` resource.
#[derive(Clone, Debug, PartialEq)]
enum DurableStatus {
    /// The answer is still in flight.
    Loading,
    Failed(String),
    /// The object holds nothing: the pull request closed and its state was retired
    /// with its row — or, rarely, the state was reset and the next sync restores it.
    Gone,
    Present(Box<PrState>),
}

impl DurableStatus {
    fn from_resource(resource: Option<&Result<Option<PrState>, ServerFnError>>) -> Self {
        match resource {
            None => Self::Loading,
            Some(Err(error)) => Self::Failed(user_facing(error)),
            Some(Ok(None)) => Self::Gone,
            Some(Ok(Some(state))) => Self::Present(Box::new(state.clone())),
        }
    }
}

/// The drawer's durable-state section; `now` is the clock its times are
/// read against.
#[component]
fn DurableState(status: DurableStatus, now: u64) -> Element {
    match status {
        DurableStatus::Loading => rsx! {
            div { class: "loading-state",
                Loading { size: LoadingSize::Sm }
                "Reading durable activity"
            }
        },
        DurableStatus::Failed(error) => rsx! {
            p { class: "batch-failure", "Could not load activity: {error}" }
        },
        DurableStatus::Gone => rsx! {
            p { class: "durable-state-gone",
                "No durable state: this pull request is no longer open, or has not been synced since its state was reset."
            }
        },
        DurableStatus::Present(state) => rsx! {
            dl { class: "detail-list",
                dt { "Last canonical sync" }
                dd {
                    if let Some(last_synced_at) = state.last_synced_at {
                        "{relative_time(now, last_synced_at)}"
                    } else {
                        "not yet"
                    }
                }
                dt { "Debounced sync" }
                dd { if state.sync_pending { "pending" } else { "idle" } }
            }
            div { class: "dependency-list",
                if state.history.is_empty() {
                    div { "No durable activity recorded." }
                } else {
                    for entry in state.history.iter().rev() {
                        div {
                            strong { "{entry.action}" }
                            code { "{relative_time(now, entry.at)}" }
                            span { "{entry.detail}" }
                        }
                    }
                }
            }
        },
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::{Capabilities, DashboardPage, Mergeable, PrFilter};
    use dioxus::core::{ElementId, Mutation, consume_context_from_scope};

    use super::*;
    use crate::ui::dashboard_state::{
        Answers, CapabilitiesStatus, DashboardState, PageStatus, REBASE_UNAVAILABLE, Selection,
        SummaryStatus,
    };
    use crate::ui::test_support::{
        DashboardFixture, FIXTURE_NOW, grouped_row, loaded_page, off_page_row, render, serde_row,
    };

    #[test]
    fn the_drawer_is_there_for_the_open_pull_request_and_gone_when_none_is() {
        fn Closed() -> Element {
            let detail = use_signal(|| None);
            rsx! {
                DashboardFixture {
                    OpenDetail { detail, now: FIXTURE_NOW, onaction: move |_| {}, onsync: move |_| {} }
                }
            }
        }
        let closed = render(Closed);
        assert!(!closed.contains("side-drawer"), "{closed}");

        fn Open() -> Element {
            let detail = use_signal(|| Some(OpenPr::from(grouped_row())));
            rsx! {
                DashboardFixture {
                    OpenDetail { detail, now: FIXTURE_NOW, onaction: move |_| {}, onsync: move |_| {} }
                }
            }
        }
        let open = render(Open);
        assert!(
            open.contains(r#"class="side-drawer detail-drawer""#),
            "{open}"
        );
        assert!(open.contains("acme/api#9"), "{open}");
    }

    /// The drawer reads the pull request's durable state when it is mounted;
    /// so opening another pull request has to mount a fresh drawer rather
    /// than hand the old one a new row. A fresh drawer shows as one of the
    /// roots mounted the first time round — the old drawer's scrim — leaving
    /// the DOM, which a drawer merely handed a new row would not do.
    #[test]
    fn opening_another_pull_request_mounts_a_fresh_drawer() {
        fn Fixture() -> Element {
            let detail = use_context_provider(|| Signal::new(Some(OpenPr::from(grouped_row()))));
            rsx! {
                DashboardFixture {
                    OpenDetail { detail, now: FIXTURE_NOW, onaction: move |_| {}, onsync: move |_| {} }
                }
            }
        }
        let mut dom = VirtualDom::new(Fixture);
        let mounted = dom.rebuild_to_vec();
        let roots: Vec<ElementId> = mounted
            .edits
            .iter()
            .filter_map(|edit| match edit {
                Mutation::LoadTemplate { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert!(!roots.is_empty(), "the drawer was mounted: {mounted:?}");
        let mut detail = dom
            .in_runtime(|| consume_context_from_scope::<Signal<Option<OpenPr>>>(ScopeId::APP))
            .expect("the fixture provides the open pull request");

        dom.in_runtime(|| detail.set(Some(OpenPr::from(serde_row()))));
        let mutations = dom.render_immediate_to_vec();

        let a_root_left = mutations.edits.iter().any(|edit| {
            matches!(
                edit,
                Mutation::Remove { id } | Mutation::ReplaceWith { id, .. } if roots.contains(id)
            )
        });
        assert!(a_root_left, "{mutations:?}");
    }

    /// The drawer is a view of the read model, not a snapshot of it: when the
    /// page reloads with the open pull request's row changed — a live
    /// refresh after a push moved its head — the drawer shows the new row,
    /// so the head SHA it would submit is the one on screen.
    #[test]
    fn a_reload_that_changes_the_open_pull_requests_row_reaches_the_drawer() {
        let mut dom = VirtualDom::new(FollowsThePage);
        dom.rebuild_in_place();
        settle(&mut dom);
        let before = dioxus::ssr::render(&dom);
        assert!(before.contains("<code>abc123</code>"), "{before}");

        let mut page = dom
            .in_runtime(|| consume_context_from_scope::<Signal<PageStatus>>(ScopeId::APP))
            .expect("the fixture provides the page");
        let mut moved = grouped_row();
        moved.head_sha = "f00d1e".to_owned();
        dom.in_runtime(|| {
            page.set(PageStatus::Loaded(DashboardPage {
                rows: vec![moved.clone(), serde_row()],
                ..loaded_page()
            }))
        });
        settle(&mut dom);

        let detail = dom
            .in_runtime(|| consume_context_from_scope::<Signal<Option<OpenPr>>>(ScopeId::APP))
            .expect("the fixture provides the open pull request");
        assert_eq!(
            dom.in_runtime(|| detail.read().clone()),
            Some(OpenPr::from(moved))
        );
        let after = dioxus::ssr::render(&dom);
        assert!(after.contains("<code>f00d1e</code>"), "{after}");
        assert!(!after.contains("<code>abc123</code>"), "{after}");
        assert!(!after.contains("no longer open"), "{after}");
    }

    /// [`OpenDetail`] over a dashboard whose page is on the app scope, so a
    /// test can reload it; the open pull request is [`grouped_row`], which
    /// the page shows.
    fn FollowsThePage() -> Element {
        let page = use_context_provider(|| Signal::new(PageStatus::Loaded(loaded_page())));
        let detail = use_context_provider(|| Signal::new(Some(OpenPr::from(grouped_row()))));
        DashboardState::provide(
            use_signal(PrFilter::default),
            use_signal(|| None),
            use_signal(Selection::default),
            Answers {
                page: page.into(),
                summary: use_signal(|| SummaryStatus::Loading).into(),
                capabilities: use_signal(|| CapabilitiesStatus::Loading).into(),
            },
            use_signal(|| FIXTURE_NOW),
            use_callback(|_| {}),
        );
        rsx! {
            OpenDetail { detail, now: FIXTURE_NOW, onaction: move |_| {}, onsync: move |_| {} }
        }
    }

    /// Lets pending tasks and effects run. An effect that writes a signal
    /// queues the next round, so a few rounds cover the cascade.
    fn settle(dom: &mut VirtualDom) {
        for _ in 0..4 {
            dom.render_immediate_to_vec();
        }
    }

    /// What a reloaded page says about the open pull request: the same row
    /// is nothing new; a changed row replaces the drawer's, as does a row
    /// back on the page after the drawer had given it up for gone; a row the
    /// page does not show says nothing either way, since the page is one
    /// slice of one filter, and the pull request has to be asked about — but
    /// not one already given up for gone, which only a page showing it again
    /// brings back. A pull request still being read has no row to follow.
    #[test]
    fn the_page_says_whether_the_open_row_stands_has_changed_or_is_not_there() {
        let page = loaded_page();
        let open = OpenPr::from(grouped_row());
        assert_eq!(follow(&open, &page), Followed::AsIs);

        let mut moved = grouped_row();
        moved.head_sha = "f00d1e".to_owned();
        let reloaded = DashboardPage {
            rows: vec![moved.clone(), serde_row()],
            ..loaded_page()
        };
        assert_eq!(
            follow(&open, &reloaded),
            Followed::Replaced(Box::new(moved.clone()))
        );

        let given_up = OpenPr::Gone(Box::new(grouped_row()));
        assert_eq!(
            follow(&given_up, &page),
            Followed::Replaced(Box::new(grouped_row())),
            "a row back on the page is open again"
        );
        let without = DashboardPage {
            rows: vec![serde_row()],
            ..loaded_page()
        };
        assert_eq!(
            follow(&given_up, &without),
            Followed::AsIs,
            "a pull request given up for gone is not asked about again"
        );

        assert_eq!(
            follow(&OpenPr::from(off_page_row()), &page),
            Followed::Unknown(Box::new(off_page_row()))
        );
        assert_eq!(
            follow(&OpenPr::Loading(PrKey::new(7, 9)), &page),
            Followed::AsIs
        );
    }

    fn DrawerFixture() -> Element {
        let mut row = grouped_row();
        // `has_hooks` is the one state whose display form differs from its
        // variant name, so the assertion below can tell Display from Debug.
        row.mergeable = Mergeable::HasHooks;
        rsx! {
            DetailDrawer {
                row,
                now: FIXTURE_NOW,
                onclose: move |_| {},
                onaction: move |_| {},
                onsync: move |_| {},
            }
        }
    }

    #[test]
    fn detail_drawer_shows_the_mergeable_state_in_its_display_form() {
        let mut dom = VirtualDom::new(DrawerFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        assert!(
            html.contains(r#"<span class="status-badge">has_hooks</span>"#),
            "{html}"
        );
    }

    #[test]
    fn detail_drawer_offers_rebase_update_branch_and_merge_for_its_pull_request() {
        let mut dom = VirtualDom::new(DrawerFixture);
        dom.rebuild_in_place();
        let html = dioxus::ssr::render(&dom);

        for (class, label) in [
            ("rebase-button", ">Rebase<"),
            ("update-branch-button", ">Update branch<"),
            ("merge-button", ">Merge<"),
        ] {
            assert!(html.contains(class), "{class}: {html}");
            assert!(html.contains(label), "{label}: {html}");
        }
        assert!(!html.contains("disabled"), "{html}");
        assert!(!html.contains("no longer open"), "{html}");
    }

    /// A pull request that has left the read model since its drawer opened
    /// is still shown, as the drawer last saw it, but said to be no longer
    /// open, and every action is withheld: the bulk actions would be rejected
    /// against a head that is gone, and a sync starts from the row in the
    /// read model, which is no longer there.
    #[test]
    fn a_pull_request_that_is_gone_is_said_to_be_no_longer_open_and_offers_no_action() {
        fn Fixture() -> Element {
            let detail = use_signal(|| Some(OpenPr::Gone(Box::new(grouped_row()))));
            // The page after the merge: the row has left it.
            let page = PageStatus::Loaded(DashboardPage {
                rows: vec![serde_row()],
                total: 51,
                ..loaded_page()
            });
            rsx! {
                DashboardFixture { page,
                    OpenDetail { detail, now: FIXTURE_NOW, onaction: move |_| {}, onsync: move |_| {} }
                }
            }
        }
        let html = render(Fixture);

        assert!(html.contains("acme/api#9"), "{html}");
        assert!(
            html.contains(r#"<p class="drawer-gone">This pull request is no longer open."#),
            "{html}"
        );
        assert_eq!(
            html.matches("disabled").count(),
            4,
            "rebase, update branch, merge, and sync are withheld: {html}"
        );
    }

    /// Once the service has said it has no user token to post `@dependabot
    /// rebase` with, the drawer withholds that one action with the reason,
    /// and offers the rest: update branch and merge run as the App, and a
    /// sync needs no identity at all. Until it has said so, the rebase is on
    /// offer like the rest, and the service guards the command itself.
    #[test]
    fn a_rebase_is_withheld_with_its_reason_once_the_service_says_it_is_off() {
        #[component]
        fn Fixture(capabilities: CapabilitiesStatus) -> Element {
            let detail = use_signal(|| Some(OpenPr::from(grouped_row())));
            rsx! {
                DashboardFixture { page: PageStatus::Loaded(loaded_page()), capabilities,
                    OpenDetail { detail, now: FIXTURE_NOW, onaction: move |_| {}, onsync: move |_| {} }
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
        assert!(off.contains(r#"rebase-button" disabled=true"#), "{off}");
        assert_eq!(
            off.matches("disabled").count(),
            1,
            "the rebase alone is withheld: {off}"
        );

        let unknown = render(CapabilitiesStatus::Loading);
        assert!(!unknown.contains("disabled"), "{unknown}");
        assert!(!unknown.contains(REBASE_UNAVAILABLE), "{unknown}");
    }

    fn render_durable_state(status: DurableStatus) -> String {
        let mut dom = VirtualDom::new_with_props(
            DurableState,
            DurableStateProps {
                status,
                now: FIXTURE_NOW,
            },
        );
        dom.rebuild_in_place();
        dioxus::ssr::render(&dom)
    }

    #[test]
    fn durable_state_reports_a_pull_request_that_is_no_longer_open() {
        let html = render_durable_state(DurableStatus::Gone);

        assert!(html.contains("no longer open"), "{html}");
        assert!(!html.contains("Reading durable activity"), "{html}");
    }

    #[test]
    fn durable_state_keeps_reading_while_the_answer_is_in_flight() {
        let html = render_durable_state(DurableStatus::Loading);

        assert!(html.contains("Reading durable activity"), "{html}");
        assert!(!html.contains("no longer open"), "{html}");
    }

    #[test]
    fn an_empty_status_answer_means_the_state_is_gone_not_still_loading() {
        assert_eq!(
            DurableStatus::from_resource(Some(&Ok(None))),
            DurableStatus::Gone
        );
        assert_eq!(DurableStatus::from_resource(None), DurableStatus::Loading);
    }
}
