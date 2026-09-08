//! The fixtures the store's tests share: a store on a temporary directory, a
//! second connection to the same file standing in for the other process, and
//! a repository and a pull request of installation 9.

use dependaboard_core::{
    CheckStatus, DependencyUpdate, Mergeable, PrKey, PrRecord, RepoRecord, UpdateType,
};
use libsql::Builder;
use tempfile::TempDir;

use crate::{LibSqlPrStore, StoreConfig};

pub(crate) async fn test_store() -> (TempDir, LibSqlPrStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = LibSqlPrStore::connect(&StoreConfig::local(database_path(&directory)))
        .await
        .unwrap();
    (directory, store)
}

pub(crate) fn database_path(directory: &TempDir) -> std::path::PathBuf {
    directory.path().join("db.sqlite")
}

/// An independent connection to the test database, standing in for the
/// other process (web or Restate) that shares the file in production.
pub(crate) async fn sidecar(directory: &TempDir) -> libsql::Connection {
    Builder::new_local(database_path(directory))
        .build()
        .await
        .unwrap()
        .connect()
        .unwrap()
}

pub(crate) fn repo(id: u64, synced_at: u64) -> RepoRecord {
    RepoRecord {
        repository_id: id,
        installation_id: 9,
        owner: "acme".to_owned(),
        repo: format!("repo-{id}"),
        merge_method: None,
        synced_at,
    }
}

pub(crate) fn pr(repository_id: u64, number: u64, synced_at: u64) -> PrRecord {
    PrRecord {
        id: PrKey::new(repository_id, number).to_string(),
        repository_id,
        installation_id: 9,
        owner: "acme".to_owned(),
        repo: format!("repo-{repository_id}"),
        number,
        title: "Bump serde from 1.0.0 to 1.1.0".to_owned(),
        html_url: format!("https://github.com/acme/repo-{repository_id}/pull/{number}"),
        dependency: Some("serde".to_owned()),
        from_version: Some("1.0.0".to_owned()),
        to_version: Some("1.1.0".to_owned()),
        dependencies: vec![DependencyUpdate {
            name: "serde".to_owned(),
            from_version: Some("1.0.0".to_owned()),
            to_version: Some("1.1.0".to_owned()),
            update_type: UpdateType::Minor,
        }],
        update_type: UpdateType::Minor,
        head_sha: format!("sha-{number}"),
        check_status: CheckStatus::Success,
        mergeable: Mergeable::Clean,
        labels: vec!["dependencies".to_owned(), "rust".to_owned()],
        created_at: 10,
        updated_at: 20 + number,
        synced_at,
    }
}
