//! Shared fixtures for the UI modules' SSR snapshot tests.

use std::collections::{BTreeMap, BTreeSet};

use dependaboard_core::{
    CheckStatus, DashboardPage, DashboardSummary, DependencyUpdate, FacetCounts, LabelFacet,
    Mergeable, PrFilter, PrRecord, RepoFacet, RepoRecord, UpdateType, unix_seconds,
};
use dioxus::prelude::*;

use crate::components::toast::ToastProvider;
use crate::ui::dashboard_state::{DashboardState, PageStatus, SummaryStatus};

pub(crate) const GROUPED_ROW_TITLE: &str =
    "build(deps): bump the github-actions group across 1 directory with 3 updates";

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
        updated_at: 1,
        synced_at: unix_seconds(),
    }
}

/// A single-dependency row in a second repository, `acme/web`.
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
        ..grouped_row()
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
        last_synced_at: Some(unix_seconds()),
    }
}

/// Mounts `children` where the dashboard's components expect to be: under a
/// toast provider and a dashboard state with `filter` in force, `page` and
/// `summary` as the read model's answers, and `selected` picked. Reloads go
/// nowhere.
#[component]
pub(crate) fn DashboardFixture(
    #[props(default)] filter: PrFilter,
    #[props(default = PageStatus::Loading)] page: PageStatus,
    #[props(default = SummaryStatus::Loading)] summary: SummaryStatus,
    #[props(default)] selected: BTreeSet<String>,
    children: Element,
) -> Element {
    DashboardState::provide(
        use_signal(|| filter),
        use_signal(|| None),
        use_signal(|| selected),
        use_signal(|| page),
        use_signal(|| summary),
        use_callback(|_| {}),
    );
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
