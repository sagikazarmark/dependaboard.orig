//! The drawer for one pull request: its projection, durable state, and the
//! per-row rebase, merge, and sync actions. A pull request a link names by
//! key alone has its row read here before the drawer opens.

use std::time::Duration;

use dependaboard_core::{BulkActionKind, PrKey, PrRecord, PrState, unix_seconds};
use dioxus::logger::tracing;
use dioxus::prelude::*;

use crate::api::{load_pr_projection, load_pr_status, request_pr_sync};
use crate::components::button::{Button, ButtonSize};
use crate::components::loading::{Loading, LoadingSize};
use crate::ui::format::{relative_time, status_class, status_label, update_class, version_label};
use crate::ui::side_panel::SidePanel;
use crate::ui::{POLL_INTERVAL, PendingAction, pr_target, sleep, user_facing};

/// How long the drawer waits for a manual sync to complete before it stops
/// polling for it.
const SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// [`SYNC_TIMEOUT`] in polls.
const SYNC_POLLS: u64 = SYNC_TIMEOUT.as_secs() / POLL_INTERVAL.as_secs();

/// The pull request whose drawer is open. A row the user clicked is in hand
/// at once; a link names the pull request by key alone, and the drawer waits
/// for its row to be read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OpenPr {
    Loading(PrKey),
    Loaded(Box<PrRecord>),
}

impl OpenPr {
    pub(crate) fn key(&self) -> PrKey {
        match self {
            Self::Loading(key) => key.clone(),
            Self::Loaded(row) => row.key(),
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
/// was made, or cannot be read, nothing opens.
#[component]
pub(crate) fn OpenDetail(
    mut detail: Signal<Option<OpenPr>>,
    onaction: EventHandler<PendingAction>,
    onsync: EventHandler<Result<Option<PrRecord>, String>>,
) -> Element {
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
    let open = detail.read();
    rsx! {
        if let Some(OpenPr::Loaded(row)) = &*open {
            DetailDrawer {
                key: "{row.id}",
                row: (**row).clone(),
                onclose: move |_| detail.set(None),
                onaction,
                onsync,
            }
        }
    }
}

#[component]
pub(crate) fn DetailDrawer(
    row: PrRecord,
    onclose: EventHandler<()>,
    onaction: EventHandler<PendingAction>,
    onsync: EventHandler<Result<Option<PrRecord>, String>>,
) -> Element {
    let stale = row.is_stale(unix_seconds());
    let mut syncing = use_signal(|| false);
    let mut sync_queued = use_signal(|| false);
    let mut status = use_resource({
        let repository_id = row.repository_id;
        let number = row.number;
        move || load_pr_status(repository_id, number)
    });
    let durable_state = DurableStatus::from_resource(status.read().as_ref());
    let sync_repository_id = row.repository_id;
    let sync_number = row.number;
    let target = pr_target(&row);
    let request = use_callback(move |action: BulkActionKind| {
        onaction.call(PendingAction {
            action,
            targets: vec![target.clone()],
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
                        onclick: move |_| request(BulkActionKind::Rebase),
                        "Rebase"
                    }
                    Button {
                        size: ButtonSize::Sm,
                        class: "merge-button",
                        onclick: move |_| request(BulkActionKind::Merge),
                        "Merge"
                    }
                    Button {
                        size: ButtonSize::Sm,
                        class: "drawer-sync",
                        disabled: syncing() || sync_queued(),
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
                                                status.restart();
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
                dl { class: "detail-list",
                    dt { "Head SHA" } dd { code { "{row.head_sha}" } }
                    dt { "Last updated" } dd { "{relative_time(row.updated_at)}" }
                    dt { "Projected" } dd { "{relative_time(row.synced_at)}" }
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
                DurableState { status: durable_state }
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

/// The drawer's durable-state section.
#[component]
fn DurableState(status: DurableStatus) -> Element {
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
                        "{relative_time(last_synced_at)}"
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
                            code { "{relative_time(entry.at)}" }
                            span { "{entry.detail}" }
                        }
                    }
                }
            }
        },
    }
}

async fn wait_for_pr_sync_completion(
    repository_id: u64,
    number: u64,
    completion_id: String,
) -> Result<Option<PrRecord>, String> {
    let mut last_error = None;
    for _ in 0..SYNC_POLLS {
        sleep(POLL_INTERVAL).await;
        match load_pr_status(repository_id, number).await {
            Ok(state) if sync_id_completed(state.as_ref(), &completion_id) => {
                match load_pr_projection(repository_id, number).await {
                    Ok(row) => return Ok(row),
                    Err(error) => last_error = Some(user_facing(&error)),
                }
            }
            Ok(_) => match load_pr_projection(repository_id, number).await {
                Ok(None) => return Ok(None),
                Ok(Some(_)) => last_error = None,
                Err(error) => last_error = Some(user_facing(&error)),
            },
            Err(error) => last_error = Some(user_facing(&error)),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        format!(
            "the sync did not complete within {} seconds",
            SYNC_TIMEOUT.as_secs()
        )
    }))
}

fn sync_id_completed(state: Option<&PrState>, completion_id: &str) -> bool {
    state.is_some_and(|state| {
        state
            .completed_sync_ids
            .iter()
            .any(|completed| completed == completion_id)
    })
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use dependaboard_core::Mergeable;
    use dioxus::core::{Mutation, consume_context_from_scope};

    use super::*;
    use crate::ui::test_support::{grouped_row, render, serde_row};

    #[test]
    fn the_drawer_is_there_for_the_open_pull_request_and_gone_when_none_is() {
        fn Closed() -> Element {
            let detail = use_signal(|| None);
            rsx! {
                OpenDetail { detail, onaction: move |_| {}, onsync: move |_| {} }
            }
        }
        let closed = render(Closed);
        assert_eq!(closed, "", "{closed}");

        fn Open() -> Element {
            let detail = use_signal(|| Some(OpenPr::from(grouped_row())));
            rsx! {
                OpenDetail { detail, onaction: move |_| {}, onsync: move |_| {} }
            }
        }
        let open = render(Open);
        assert!(
            open.contains(r#"class="side-drawer detail-drawer""#),
            "{open}"
        );
        assert!(open.contains("acme/api#9"), "{open}");
    }

    /// The drawer reads the pull request's durable state once, when it is
    /// mounted; so opening another pull request has to mount a fresh drawer
    /// rather than hand the old one a new row. A fresh drawer shows as the
    /// old drawer's scrim, the first element mounted, leaving the DOM.
    #[test]
    fn opening_another_pull_request_mounts_a_fresh_drawer() {
        fn Fixture() -> Element {
            let detail = use_context_provider(|| Signal::new(Some(OpenPr::from(grouped_row()))));
            rsx! {
                OpenDetail { detail, onaction: move |_| {}, onsync: move |_| {} }
            }
        }
        let mut dom = VirtualDom::new(Fixture);
        let mounted = dom.rebuild_to_vec();
        let scrim = mounted
            .edits
            .iter()
            .find_map(|edit| match edit {
                Mutation::LoadTemplate { id, .. } => Some(*id),
                _ => None,
            })
            .expect("the drawer was mounted");
        let mut detail = dom
            .in_runtime(|| consume_context_from_scope::<Signal<Option<OpenPr>>>(ScopeId::APP))
            .expect("the fixture provides the open pull request");

        dom.in_runtime(|| detail.set(Some(OpenPr::from(serde_row()))));
        let mutations = dom.render_immediate_to_vec();

        let scrim_left = mutations.edits.iter().any(|edit| {
            matches!(
                edit,
                Mutation::Remove { id } | Mutation::ReplaceWith { id, .. } if *id == scrim
            )
        });
        assert!(scrim_left, "{mutations:?}");
    }

    fn DrawerFixture() -> Element {
        let mut row = grouped_row();
        // `has_hooks` is the one state whose display form differs from its
        // variant name, so the assertion below can tell Display from Debug.
        row.mergeable = Mergeable::HasHooks;
        rsx! {
            DetailDrawer {
                row,
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

    fn render_durable_state(status: DurableStatus) -> String {
        let mut dom = VirtualDom::new_with_props(DurableState, DurableStateProps { status });
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

    #[test]
    fn pull_request_sync_completes_only_for_its_request_id() {
        let mut state = PrState::default();
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-456".to_owned());
        assert!(!sync_id_completed(Some(&state), "sync-123"));
        state.complete_sync("sync-123".to_owned());
        assert!(sync_id_completed(Some(&state), "sync-123"));
    }
}
