//! Shared fixtures for the UI modules' SSR snapshot tests.

use dependaboard_core::{CheckStatus, Mergeable, PrRecord, UpdateType, unix_seconds};

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
            .map(|name| dependaboard_core::DependencyUpdate {
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
