//! An in-memory [`PrStore`] that keeps the libSQL schema's constraints, so handler logic
//! that takes the trait can run against a projection without a database.

use std::{collections::BTreeMap, sync::Mutex};

use async_trait::async_trait;
use dependaboard_core::{
    DashboardPage, DashboardSummary, Page, PrFilter, PrKey, PrRecord, RepoRecord,
};
use dependaboard_store::{PrStore, StoreError};

/// Rows keyed the way the schema keys them: pull requests by `(repository_id, number)`
/// (the `id` column is derived from that pair), repositories by id.
///
/// What the schema enforces, this enforces: a pull request needs its repository's row
/// first (libSQL rejects the foreign key; this panics, since only a test can get it wrong),
/// deleting a repository takes its pull requests with it, and a pull request reads back
/// with its repository's `installation_id`, as the store's `JOIN` gives it. What the
/// service never asks of the store, the dashboard listing and its summary, is not
/// implemented.
#[derive(Default)]
pub(crate) struct MemoryPrStore {
    tables: Mutex<Tables>,
}

#[derive(Default)]
struct Tables {
    pull_requests: BTreeMap<(u64, u64), PrRecord>,
    repositories: BTreeMap<u64, RepoRecord>,
}

impl Tables {
    /// The pull request as the store's `JOIN` would read it: `installation_id` comes from
    /// the repository row, not from what was upserted.
    fn joined(&self, pull: &PrRecord) -> PrRecord {
        let installation_id = self.repositories[&pull.repository_id].installation_id;
        PrRecord {
            installation_id,
            ..pull.clone()
        }
    }

    /// Drops the pull requests of `repository_ids`; resolves to their keys in key order.
    fn cascade(&mut self, repository_ids: &[u64]) -> Vec<PrKey> {
        let purged = self
            .pull_requests
            .keys()
            .filter(|(repository_id, _)| repository_ids.contains(repository_id))
            .copied()
            .collect::<Vec<_>>();
        for key in &purged {
            self.pull_requests.remove(key);
        }
        purged
            .into_iter()
            .map(|(repository_id, number)| PrKey::new(repository_id, number))
            .collect()
    }

    fn retain_repos(&mut self, installation_id: u64, live: &[u64], synced_before: u64) -> u64 {
        let stale = self
            .repositories
            .values()
            .filter(|repository| {
                repository.installation_id == installation_id
                    && !live.contains(&repository.repository_id)
                    && repository.synced_at < synced_before
            })
            .map(|repository| repository.repository_id)
            .collect::<Vec<_>>();
        for repository_id in &stale {
            self.repositories.remove(repository_id);
        }
        self.cascade(&stale);
        stale.len() as u64
    }
}

#[async_trait]
impl PrStore for MemoryPrStore {
    async fn upsert_pr(&self, pr: &PrRecord) -> Result<(), StoreError> {
        let mut tables = self.tables.lock().unwrap();
        assert!(
            tables.repositories.contains_key(&pr.repository_id),
            "MemoryPrStore: pull request {} references repository {}, which has no row; \
             upsert the repository first, as the schema's foreign key requires",
            pr.key(),
            pr.repository_id
        );
        tables
            .pull_requests
            .insert((pr.repository_id, pr.number), pr.clone());
        Ok(())
    }

    async fn get_pr(&self, key: &PrKey) -> Result<Option<PrRecord>, StoreError> {
        let tables = self.tables.lock().unwrap();
        Ok(tables
            .pull_requests
            .get(&(key.repository_id, key.number))
            .map(|pull| tables.joined(pull)))
    }

    async fn delete_pr(&self, key: &PrKey) -> Result<(), StoreError> {
        self.tables
            .lock()
            .unwrap()
            .pull_requests
            .remove(&(key.repository_id, key.number));
        Ok(())
    }

    async fn retain_prs(
        &self,
        repository_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError> {
        let mut tables = self.tables.lock().unwrap();
        let stale = tables
            .pull_requests
            .iter()
            .filter(|((repository, number), pull)| {
                *repository == repository_id
                    && !live.contains(number)
                    && pull.synced_at < synced_before
            })
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for key in &stale {
            tables.pull_requests.remove(key);
        }
        Ok(stale
            .into_iter()
            .map(|(repository_id, number)| PrKey::new(repository_id, number))
            .collect())
    }

    async fn list_prs(&self, _filter: &PrFilter, _page: Page) -> Result<DashboardPage, StoreError> {
        unimplemented!(
            "the Restate service never lists the projection; the dashboard reads it through the web app"
        )
    }

    async fn dashboard_summary(&self, _filter: &PrFilter) -> Result<DashboardSummary, StoreError> {
        unimplemented!(
            "the Restate service never summarises the projection; the dashboard reads it through the web app"
        )
    }

    async fn projection_revision(&self) -> Result<u64, StoreError> {
        unimplemented!(
            "the Restate service never asks whether the projection moved; the dashboard polls it through the web app"
        )
    }

    async fn prs_for_sha(
        &self,
        repository_id: u64,
        sha: &str,
    ) -> Result<Vec<PrRecord>, StoreError> {
        let tables = self.tables.lock().unwrap();
        Ok(tables
            .pull_requests
            .values()
            .filter(|pull| pull.repository_id == repository_id && pull.head_sha == sha)
            .map(|pull| tables.joined(pull))
            .collect())
    }

    async fn upsert_repo(&self, repo: &RepoRecord) -> Result<(), StoreError> {
        self.tables
            .lock()
            .unwrap()
            .repositories
            .insert(repo.repository_id, repo.clone());
        Ok(())
    }

    async fn get_repo(&self, repository_id: u64) -> Result<Option<RepoRecord>, StoreError> {
        Ok(self
            .tables
            .lock()
            .unwrap()
            .repositories
            .get(&repository_id)
            .cloned())
    }

    async fn replace_installation_repos(
        &self,
        installation_id: u64,
        repos: &[RepoRecord],
        synced_before: u64,
    ) -> Result<u64, StoreError> {
        let mut tables = self.tables.lock().unwrap();
        for repo in repos {
            tables.repositories.insert(repo.repository_id, repo.clone());
        }
        let live = repos
            .iter()
            .map(|repo| repo.repository_id)
            .collect::<Vec<_>>();
        Ok(tables.retain_repos(installation_id, &live, synced_before))
    }

    async fn retain_repos(
        &self,
        installation_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<u64, StoreError> {
        Ok(self
            .tables
            .lock()
            .unwrap()
            .retain_repos(installation_id, live, synced_before))
    }

    async fn purge_installation(&self, installation_id: u64) -> Result<Vec<PrKey>, StoreError> {
        let mut tables = self.tables.lock().unwrap();
        let repositories = tables
            .repositories
            .values()
            .filter(|repository| repository.installation_id == installation_id)
            .map(|repository| repository.repository_id)
            .collect::<Vec<_>>();
        let purged = tables.cascade(&repositories);
        for repository_id in &repositories {
            tables.repositories.remove(repository_id);
        }
        Ok(purged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{repository, snapshot};

    fn pull(repository_id: u64, number: u64, synced_at: u64) -> PrRecord {
        PrRecord {
            id: PrKey::new(repository_id, number).to_string(),
            repository_id,
            number,
            synced_at,
            ..snapshot()
        }
    }

    #[tokio::test]
    async fn retention_prunes_only_rows_absent_from_the_listing_and_synced_before_it_started() {
        let store = MemoryPrStore::default();
        store.upsert_repo(&repository()).await.unwrap();
        store
            .upsert_repo(&RepoRecord {
                repository_id: 8,
                ..repository()
            })
            .await
            .unwrap();
        for pull in [
            pull(7, 1, 500),
            pull(7, 2, 500),
            pull(7, 3, 1_000),
            pull(8, 1, 500),
        ] {
            store.upsert_pr(&pull).await.unwrap();
        }

        let pruned = store.retain_prs(7, &[1], 1_000).await.unwrap();

        assert_eq!(pruned, vec![PrKey::new(7, 2)]);
        assert!(store.get_pr(&PrKey::new(7, 1)).await.unwrap().is_some());
        assert!(
            store.get_pr(&PrKey::new(7, 3)).await.unwrap().is_some(),
            "a webhook synced it since the listing started; it may have been reopened"
        );
        assert!(
            store.get_pr(&PrKey::new(8, 1)).await.unwrap().is_some(),
            "another repository's rows are none of this sweep's business"
        );
    }

    #[tokio::test]
    async fn purging_an_installation_takes_its_pull_requests_with_its_repositories() {
        let store = MemoryPrStore::default();
        store.upsert_repo(&repository()).await.unwrap();
        store
            .upsert_repo(&RepoRecord {
                repository_id: 8,
                installation_id: 2,
                ..repository()
            })
            .await
            .unwrap();
        store.upsert_pr(&pull(7, 9, 0)).await.unwrap();
        store.upsert_pr(&pull(7, 3, 0)).await.unwrap();
        store.upsert_pr(&pull(8, 1, 0)).await.unwrap();

        let purged = store.purge_installation(1).await.unwrap();

        assert_eq!(purged, vec![PrKey::new(7, 3), PrKey::new(7, 9)]);
        assert!(store.get_repo(7).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(7, 9)).await.unwrap().is_none());
        assert!(store.get_repo(8).await.unwrap().is_some());
        assert_eq!(
            store
                .get_pr(&PrKey::new(8, 1))
                .await
                .unwrap()
                .map(|pull| pull.installation_id),
            Some(2),
            "a pull request reads back with its repository's installation, as the JOIN gives it"
        );
    }
}
