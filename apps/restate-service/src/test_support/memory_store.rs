//! An in-memory [`ProjectionWriter`] that keeps the libSQL schema's constraints, so
//! handler logic that takes the trait can run against a projection without a database.
//!
//! It implements the half of the store the Restate service holds, and only that: the
//! dashboard's reads are `ProjectionReader`'s, which the service never names, so this is
//! not a store for a test of the web app, which reads the projection through that half.

use std::{collections::BTreeMap, sync::Mutex};

use async_trait::async_trait;
use dependaboard_core::{BatchRecord, PrKey, PrRecord, RepoRecord, Retirement, RunningBatch};
use dependaboard_store::{ProjectionWriter, StoreError};

/// Rows keyed the way the schema keys them: pull requests by `(repository_id, number)`
/// (the `id` column is derived from that pair), repositories by id, batches by batch id,
/// running and finished apart, retirements by an id that only grows.
///
/// What the schema enforces, this enforces: a pull request needs its repository's row
/// first (libSQL rejects the foreign key; this panics, since only a test can get it wrong),
/// deleting a repository takes its pull requests with it, a pull request reads back
/// with its repository's `installation_id`, as the store's `JOIN` gives it, a batch
/// recorded twice keeps its first record, recording a batch stops listing it as
/// running, and every prune queues the keys it removed for retirement under its fence.
#[derive(Default)]
pub(crate) struct MemoryPrStore {
    tables: Mutex<Tables>,
}

#[derive(Default)]
struct Tables {
    pull_requests: BTreeMap<(u64, u64), PrRecord>,
    repositories: BTreeMap<u64, RepoRecord>,
    batches: BTreeMap<String, BatchRecord>,
    running_batches: BTreeMap<String, RunningBatch>,
    retirements: BTreeMap<u64, Retirement>,
    /// The last retirement id handed out; like `AUTOINCREMENT`, never reused.
    last_retirement_id: u64,
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

    /// Drops the pull requests of `repository_ids` and queues them for retirement under
    /// `synced_before`; resolves to their keys in key order.
    fn cascade(&mut self, repository_ids: &[u64], synced_before: Option<u64>) -> Vec<PrKey> {
        let purged = self
            .pull_requests
            .keys()
            .filter(|(repository_id, _)| repository_ids.contains(repository_id))
            .copied()
            .collect::<Vec<_>>();
        for key in &purged {
            self.pull_requests.remove(key);
        }
        let purged = purged
            .into_iter()
            .map(|(repository_id, number)| PrKey::new(repository_id, number))
            .collect::<Vec<_>>();
        self.queue_retirements(&purged, synced_before);
        purged
    }

    /// Queues `keys` for retirement under `synced_before`, as the store does in the
    /// transaction that deleted them.
    fn queue_retirements(&mut self, keys: &[PrKey], synced_before: Option<u64>) {
        for key in keys {
            self.last_retirement_id += 1;
            self.retirements.insert(
                self.last_retirement_id,
                Retirement {
                    id: self.last_retirement_id,
                    key: key.clone(),
                    synced_before,
                },
            );
        }
    }

    /// Drops the installation's stale repositories and their pull requests, as the
    /// store's FK cascade does; resolves to the pull requests' keys in key order.
    fn retain_repos(
        &mut self,
        installation_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Vec<PrKey> {
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
        self.cascade(&stale, Some(synced_before))
    }
}

#[async_trait]
impl ProjectionWriter for MemoryPrStore {
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
        let stale = stale
            .into_iter()
            .map(|(repository_id, number)| PrKey::new(repository_id, number))
            .collect::<Vec<_>>();
        tables.queue_retirements(&stale, Some(synced_before));
        Ok(stale)
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
    ) -> Result<Vec<PrKey>, StoreError> {
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

    async fn purge_installation(&self, installation_id: u64) -> Result<Vec<PrKey>, StoreError> {
        let mut tables = self.tables.lock().unwrap();
        let repositories = tables
            .repositories
            .values()
            .filter(|repository| repository.installation_id == installation_id)
            .map(|repository| repository.repository_id)
            .collect::<Vec<_>>();
        let purged = tables.cascade(&repositories, None);
        for repository_id in &repositories {
            tables.repositories.remove(repository_id);
        }
        Ok(purged)
    }

    async fn pending_retirements(&self) -> Result<Vec<Retirement>, StoreError> {
        Ok(self
            .tables
            .lock()
            .unwrap()
            .retirements
            .values()
            .cloned()
            .collect())
    }

    async fn acknowledge_retirements(&self, through: u64) -> Result<(), StoreError> {
        self.tables
            .lock()
            .unwrap()
            .retirements
            .retain(|id, _| *id > through);
        Ok(())
    }

    async fn record_batch(&self, batch: &BatchRecord) -> Result<(), StoreError> {
        let mut tables = self.tables.lock().unwrap();
        tables.running_batches.remove(&batch.batch_id);
        tables
            .batches
            .entry(batch.batch_id.clone())
            .or_insert_with(|| batch.clone());
        Ok(())
    }

    async fn start_batch(&self, batch: &RunningBatch) -> Result<(), StoreError> {
        self.tables
            .lock()
            .unwrap()
            .running_batches
            .entry(batch.batch_id.clone())
            .or_insert_with(|| batch.clone());
        Ok(())
    }

    async fn unlist_batch(&self, batch_id: &str) -> Result<(), StoreError> {
        self.tables.lock().unwrap().running_batches.remove(batch_id);
        Ok(())
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

    #[tokio::test]
    async fn replacing_the_installation_repositories_reports_the_pull_requests_that_left_with_them()
    {
        let store = MemoryPrStore::default();
        for repository_id in [7, 8] {
            store
                .upsert_repo(&RepoRecord {
                    repository_id,
                    synced_at: 100,
                    ..repository()
                })
                .await
                .unwrap();
        }
        store.upsert_pr(&pull(7, 9, 0)).await.unwrap();
        store.upsert_pr(&pull(7, 3, 0)).await.unwrap();
        store.upsert_pr(&pull(8, 1, 0)).await.unwrap();

        let cascaded = store
            .replace_installation_repos(
                1,
                &[RepoRecord {
                    repository_id: 8,
                    synced_at: 500,
                    ..repository()
                }],
                200,
            )
            .await
            .unwrap();

        assert_eq!(cascaded, vec![PrKey::new(7, 3), PrKey::new(7, 9)]);
        assert!(store.get_repo(7).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(7, 9)).await.unwrap().is_none());
        assert_eq!(
            store.get_repo(8).await.unwrap().map(|repo| repo.synced_at),
            Some(500),
            "the repository that stayed carries the fresh listing"
        );
        assert!(store.get_pr(&PrKey::new(8, 1)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn every_prune_queues_what_it_removed_with_its_fence_until_acknowledged() {
        let store = MemoryPrStore::default();
        store.upsert_repo(&repository()).await.unwrap();
        store
            .upsert_repo(&RepoRecord {
                repository_id: 8,
                ..repository()
            })
            .await
            .unwrap();
        store.upsert_pr(&pull(7, 1, 0)).await.unwrap();
        store.upsert_pr(&pull(7, 2, 0)).await.unwrap();
        store.upsert_pr(&pull(8, 1, 0)).await.unwrap();

        store.retain_prs(7, &[2], 500).await.unwrap();
        let read = store.pending_retirements().await.unwrap();
        store.purge_installation(1).await.unwrap();

        assert_eq!(
            read.iter()
                .map(|retirement| (retirement.key.clone(), retirement.synced_before))
                .collect::<Vec<_>>(),
            vec![(PrKey::new(7, 1), Some(500))]
        );
        store
            .acknowledge_retirements(read.last().unwrap().id)
            .await
            .unwrap();
        let pending = store.pending_retirements().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|retirement| (retirement.key.clone(), retirement.synced_before))
                .collect::<Vec<_>>(),
            vec![(PrKey::new(7, 2), None), (PrKey::new(8, 1), None)],
            "the purge's unfenced retirements were queued after the read and survive its acknowledgement"
        );
        assert!(pending.iter().all(|retirement| retirement.id > read[0].id));
    }
}
