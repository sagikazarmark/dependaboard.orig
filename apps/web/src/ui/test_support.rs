//! Shared fixtures for the UI modules' tests: the rows and pages the SSR
//! snapshots render, the dashboard to mount them under, and the script reader
//! the scripted servers share.

use std::collections::{BTreeMap, VecDeque};

use dependaboard_core::{
    ActionOutcome, BatchProgress, BulkActionKind, CheckStatus, DashboardPage, DashboardSummary,
    DependencyUpdate, FacetCounts, LabelFacet, Mergeable, PrFilter, PrRecord, RepoFacet,
    RepoRecord, UpdateType,
};
use dioxus::prelude::*;

use crate::components::toast::ToastProvider;
use crate::ui::batch::{Followed, Listing};
use crate::ui::dashboard_state::{
    Answers, CapabilitiesStatus, Connection, DashboardState, PageStatus, Selection, SummaryStatus,
};
use crate::ui::pr_target;

pub(crate) const GROUPED_ROW_TITLE: &str =
    "build(deps): bump the github-actions group across 1 directory with 3 updates";

/// The fixture dashboard's clock: the moment [`grouped_row`] and
/// [`loaded_summary`] were synced, so their times read as "now".
pub(crate) const FIXTURE_NOW: u64 = 1_700_000_000;

/// Batch ids as the dashboard mints them: UUIDv7s, which is what the URL
/// and the server accept as one.
pub(crate) const BATCH: &str = "01926e3a-7c1e-7b7d-9f8b-2b4c6d8e0f1a";
pub(crate) const OTHER_BATCH: &str = "01926e3a-7c1e-7b7d-9f8b-2b4c6d8e0f1b";

pub(crate) fn grouped_row() -> PrRecord {
    PrRecord {
        id: "7#9".to_owned(),
        repository_id: 7,
        installation_id: 1,
        owner: "acme".to_owned(),
        repo: "api".to_owned(),
        number: 9,
        title: GROUPED_ROW_TITLE.to_owned(),
        html_url: "https://github.example/acme/api/pull/9".to_owned(),
        dependency: None,
        from_version: None,
        to_version: None,
        dependencies: ["actions/checkout", "actions/cache", "actions/setup-rust"]
            .into_iter()
            .map(|name| DependencyUpdate {
                name: name.to_owned(),
                from_version: None,
                to_version: None,
                update_type: UpdateType::Minor,
            })
            .collect(),
        update_type: UpdateType::Minor,
        head_sha: "abc123".to_owned(),
        check_status: CheckStatus::Failure,
        mergeable: Mergeable::Clean,
        labels: vec!["dependencies".to_owned(), "github_actions".to_owned()],
        created_at: 1,
        updated_at: 2,
        synced_at: FIXTURE_NOW,
    }
}

/// A single-dependency row in a second repository, `acme/web`, updated
/// before [`grouped_row`], so the fixture page lists them in the store's
/// order: newest update first.
pub(crate) fn serde_row() -> PrRecord {
    PrRecord {
        id: "8#12".to_owned(),
        repository_id: 8,
        repo: "web".to_owned(),
        number: 12,
        title: "build(deps): bump serde from 1.0.1 to 1.0.2".to_owned(),
        html_url: "https://github.example/acme/web/pull/12".to_owned(),
        dependency: Some("serde".to_owned()),
        from_version: Some("1.0.1".to_owned()),
        to_version: Some("1.0.2".to_owned()),
        dependencies: vec![DependencyUpdate {
            name: "serde".to_owned(),
            from_version: Some("1.0.1".to_owned()),
            to_version: Some("1.0.2".to_owned()),
            update_type: UpdateType::Patch,
        }],
        update_type: UpdateType::Patch,
        head_sha: "def456".to_owned(),
        check_status: CheckStatus::Success,
        labels: vec!["dependencies".to_owned(), "rust".to_owned()],
        updated_at: 1,
        ..grouped_row()
    }
}

/// A row in `acme/web` that [`loaded_page`] does not show: one of the further
/// fifty the fixture page's total speaks of, older than the rows it shows.
pub(crate) fn off_page_row() -> PrRecord {
    PrRecord {
        id: "8#13".to_owned(),
        number: 13,
        title: "build(deps): bump tokio from 1.40.0 to 1.41.0".to_owned(),
        html_url: "https://github.example/acme/web/pull/13".to_owned(),
        dependency: Some("tokio".to_owned()),
        from_version: Some("1.40.0".to_owned()),
        to_version: Some("1.41.0".to_owned()),
        dependencies: vec![DependencyUpdate {
            name: "tokio".to_owned(),
            from_version: Some("1.40.0".to_owned()),
            to_version: Some("1.41.0".to_owned()),
            update_type: UpdateType::Minor,
        }],
        update_type: UpdateType::Minor,
        head_sha: "0ff9a6e".to_owned(),
        check_status: CheckStatus::Pending,
        updated_at: 0,
        ..serde_row()
    }
}

/// The read model's rows for two open pull requests, one per repository,
/// with a further page to load.
pub(crate) fn loaded_page() -> DashboardPage {
    DashboardPage {
        rows: vec![grouped_row(), serde_row()],
        total: 52,
        next_cursor: Some("page-2".to_owned()),
    }
}

/// A merge of [`grouped_row`] and [`serde_row`] with the first one already
/// merged and the second still queued.
pub(crate) fn half_done_merge() -> BatchProgress {
    let targets = [pr_target(&grouped_row()), pr_target(&serde_row())];
    let mut progress = BatchProgress::queued("batch-1", BulkActionKind::Merge, &targets);
    progress.record(
        &targets[0].key(),
        ActionOutcome::Succeeded {
            detail: "merged".to_owned(),
            merge_sha: None,
        },
    );
    progress
}

/// `progress` as the dashboard follows it once Restate has reported it, at
/// [`FIXTURE_NOW`], with the polls answered.
pub(crate) fn followed(progress: BatchProgress) -> Followed {
    Followed {
        batch_id: progress.batch_id.clone(),
        progress: Some(progress),
        heard: true,
        since: FIXTURE_NOW,
        trouble: None,
        listing: Listing::Unasked,
    }
}

/// The facets around [`loaded_page`]: two `acme` repositories with pull
/// requests, a third, `acme/docs`, without any, and a repository under a
/// second owner, `beta/api`.
pub(crate) fn loaded_summary() -> DashboardSummary {
    let repositories = [
        (7, "acme", "api", 31),
        (9, "acme", "docs", 0),
        (8, "acme", "web", 21),
        (10, "beta", "api", 3),
    ]
    .into_iter()
    .map(|(repository_id, owner, repo, count)| RepoFacet {
        repository: RepoRecord {
            repository_id,
            installation_id: 1,
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            merge_method: None,
            synced_at: 0,
        },
        count,
    })
    .collect();
    DashboardSummary {
        facets: FacetCounts {
            checks: BTreeMap::from([(CheckStatus::Failure, 31), (CheckStatus::Success, 21)]),
            update_types: BTreeMap::from([(UpdateType::Minor, 40), (UpdateType::Patch, 12)]),
            labels: vec![
                LabelFacet {
                    label: "dependencies".to_owned(),
                    count: 52,
                },
                LabelFacet {
                    label: "rust".to_owned(),
                    count: 12,
                },
            ],
            repositories,
        },
        last_synced_at: Some(FIXTURE_NOW),
    }
}

/// Mounts `children` where the dashboard's components expect to be: under a
/// toast provider and a dashboard state with `filter` in force, `page`,
/// `summary`, and `capabilities` as the server's answers, `selected` picked
/// (cut short by the batch limit from `capped_from` matching rows, if given),
/// the clock at `now`, a manual sync in flight if `syncing`, and the line to
/// the server as `connection` found it with the last poll answered at
/// `refreshed_at`. Reloads go nowhere.
#[component]
pub(crate) fn DashboardFixture(
    #[props(default)] filter: PrFilter,
    #[props(default = PageStatus::Loading)] page: PageStatus,
    #[props(default = SummaryStatus::Loading)] summary: SummaryStatus,
    #[props(default = CapabilitiesStatus::Loading)] capabilities: CapabilitiesStatus,
    #[props(default)] selected: Vec<PrRecord>,
    #[props(default)] capped_from: Option<u64>,
    #[props(default = FIXTURE_NOW)] now: u64,
    #[props(default)] syncing: bool,
    #[props(default = Connection::Online)] connection: Connection,
    #[props(default)] refreshed_at: Option<u64>,
    children: Element,
) -> Element {
    let mut state = DashboardState::provide(
        use_signal(|| filter),
        use_signal(|| None),
        use_signal(|| {
            let selection = Selection::of(selected);
            match capped_from {
                Some(total) => selection.cut_short_from(total),
                None => selection,
            }
        }),
        Answers {
            page: use_signal(|| page).into(),
            summary: use_signal(|| summary).into(),
            capabilities: use_signal(|| capabilities).into(),
        },
        use_signal(|| now),
        use_callback(|_| {}),
    );
    use_hook(move || {
        if syncing {
            state.begin_sync();
        }
        if let Some(answered) = refreshed_at {
            state.poll_answered(answered);
        }
        if connection != Connection::Online {
            state.poll_missed(connection);
        }
    });
    rsx! {
        ToastProvider { {children} }
    }
}

/// Renders `app` once, server side.
pub(crate) fn render(app: fn() -> Element) -> String {
    let mut dom = VirtualDom::new(app);
    dom.rebuild_in_place();
    dioxus::ssr::render(&dom)
}

/// The next answer of a scripted server: each call pops the next, and the
/// last one repeats once the script runs out, so a script can say "and then
/// this, for as long as it is asked".
pub(crate) fn next_or_repeat<T: Clone>(script: &mut VecDeque<T>) -> T {
    if script.len() > 1 {
        script.pop_front().unwrap()
    } else {
        script.front().cloned().expect("the script is not empty")
    }
}
