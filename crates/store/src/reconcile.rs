//! The prunes a reconciliation makes — the pull requests a repository listing
//! no longer shows, the repositories an installation listing no longer shows,
//! an installation lost whole — each reporting the pull requests it removed
//! and queueing them in the retirement outbox, in the transaction that removed
//! them.

use dependaboard_core::PrKey;
use libsql::Value;

use crate::{StoreError, integer, unsigned};

/// Prunes one repository's pull requests down to `live`, reporting the keys it removed
/// and queueing them for retirement under `synced_before`. Two statements, so run it
/// inside a transaction: the queue must land with the delete or not at all.
pub(crate) async fn retain_prs_on(
    connection: &libsql::Connection,
    repository_id: u64,
    live: &[u64],
    synced_before: u64,
) -> Result<Vec<PrKey>, StoreError> {
    let mut params = vec![integer(repository_id)?, integer(synced_before)?];
    let live_clause = if live.is_empty() {
        String::new()
    } else {
        let placeholders = live
            .iter()
            .map(|number| {
                params.push(integer(*number)?);
                Ok(format!("?{}", params.len()))
            })
            .collect::<Result<Vec<_>, StoreError>>()?
            .join(", ");
        format!(" AND number NOT IN ({placeholders})")
    };
    let rows = connection
        .query(
            &format!(
                "DELETE FROM pull_requests WHERE repository_id = ?1 AND synced_at < ?2{live_clause} RETURNING repository_id, number"
            ),
            params,
        )
        .await?;
    let pruned = deleted_pr_keys(rows).await?;
    queue_retirements(connection, &pruned, Some(synced_before)).await?;
    Ok(pruned)
}

/// Queues `keys` in the retirement outbox under `synced_before` as their fence, one
/// row each, in key order. Run it in the transaction that deleted them.
async fn queue_retirements(
    connection: &libsql::Connection,
    keys: &[PrKey],
    synced_before: Option<u64>,
) -> Result<(), StoreError> {
    let fence = match synced_before {
        Some(synced_before) => integer(synced_before)?,
        None => Value::Null,
    };
    for key in keys {
        connection
            .execute(
                "INSERT INTO pull_request_retirements (repository_id, number, synced_before)
                 VALUES (?1, ?2, ?3)",
                vec![
                    integer(key.repository_id)?,
                    integer(key.number)?,
                    fence.clone(),
                ],
            )
            .await?;
    }
    Ok(())
}

/// Collects a `DELETE ... RETURNING repository_id, number` over `pull_requests`
/// into keys, in a stable order. SQLite performs the delete on the first step;
/// the remaining steps only hand back the rows, which must all be drained to
/// learn every key.
async fn deleted_pr_keys(mut rows: libsql::Rows) -> Result<Vec<PrKey>, StoreError> {
    let mut keys = Vec::new();
    while let Some(row) = rows.next().await? {
        keys.push(PrKey::new(
            unsigned(row.get::<i64>(0)?)?,
            unsigned(row.get::<i64>(1)?)?,
        ));
    }
    keys.sort_by_key(|key| (key.repository_id, key.number));
    Ok(keys)
}

/// Drops the installation's repositories that are not in `live` and were synced before
/// `synced_before`, and reports the pull requests that went with them, queued for
/// retirement under that fence. Run it inside a transaction; see
/// `delete_repositories_where`.
pub(crate) async fn retain_repos_on(
    connection: &libsql::Connection,
    installation_id: u64,
    live: &[u64],
    synced_before: u64,
) -> Result<Vec<PrKey>, StoreError> {
    let mut params = vec![integer(installation_id)?, integer(synced_before)?];
    let live_clause = if live.is_empty() {
        String::new()
    } else {
        let placeholders = live
            .iter()
            .map(|repository_id| {
                params.push(integer(*repository_id)?);
                Ok(format!("?{}", params.len()))
            })
            .collect::<Result<Vec<_>, StoreError>>()?
            .join(", ");
        format!(" AND repository_id NOT IN ({placeholders})")
    };
    delete_repositories_where(
        connection,
        &format!("installation_id = ?1 AND synced_at < ?2{live_clause}"),
        params,
        Some(synced_before),
    )
    .await
}

/// Drops the repositories `predicate` selects (a `WHERE` clause over `repositories`,
/// bound to `params`) and reports the pull requests that went with them, in key order,
/// queued for retirement under `synced_before` as their fence. Three statements over
/// one predicate, so run it inside a transaction: the repository delete would cascade
/// anyway; deleting the pull requests first is what lets `RETURNING` report them, and
/// the queue must land with the delete or not at all.
pub(crate) async fn delete_repositories_where(
    connection: &libsql::Connection,
    predicate: &str,
    params: Vec<Value>,
    synced_before: Option<u64>,
) -> Result<Vec<PrKey>, StoreError> {
    let rows = connection
        .query(
            &format!(
                "DELETE FROM pull_requests
                 WHERE repository_id IN (SELECT repository_id FROM repositories WHERE {predicate})
                 RETURNING repository_id, number"
            ),
            params.clone(),
        )
        .await?;
    let cascaded = deleted_pr_keys(rows).await?;
    connection
        .execute(
            &format!("DELETE FROM repositories WHERE {predicate}"),
            params,
        )
        .await?;
    queue_retirements(connection, &cascaded, synced_before).await?;
    Ok(cascaded)
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{PrFilter, PrRecord, RepoRecord};

    use crate::{
        ProjectionReader, ProjectionWriter,
        test_support::{pr, repo, test_store},
    };

    use super::*;

    #[tokio::test]
    async fn retain_reports_exactly_the_rows_it_pruned() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_repo(&repo(2, 10)).await.unwrap();
        // Stale and absent from the listing: pruned.
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 4, 10)).await.unwrap();
        // Still listed as open: kept.
        store.upsert_pr(&pr(1, 2, 10)).await.unwrap();
        // Written by a concurrent webhook sync after the listing started: kept.
        store.upsert_pr(&pr(1, 3, 30)).await.unwrap();
        // Another repository: out of scope.
        store.upsert_pr(&pr(2, 1, 10)).await.unwrap();

        let pruned = store.retain_prs(1, &[2], 20).await.unwrap();

        assert_eq!(pruned, vec![PrKey::new(1, 1), PrKey::new(1, 4)]);
        assert!(store.get_pr(&PrKey::new(1, 1)).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(1, 4)).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(1, 2)).await.unwrap().is_some());
        assert!(store.get_pr(&PrKey::new(1, 3)).await.unwrap().is_some());
        assert!(store.get_pr(&PrKey::new(2, 1)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn purge_reports_every_pull_request_of_the_installation_and_spares_the_rest() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_repo(&repo(2, 10)).await.unwrap();
        let other_installation = RepoRecord {
            installation_id: 11,
            ..repo(3, 10)
        };
        store.upsert_repo(&other_installation).await.unwrap();
        store.upsert_pr(&pr(1, 5, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 8, 40)).await.unwrap();
        store.upsert_pr(&pr(2, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(3, 2, 10)).await.unwrap();

        let purged = store.purge_installation(9).await.unwrap();

        assert_eq!(
            purged,
            vec![PrKey::new(1, 5), PrKey::new(1, 8), PrKey::new(2, 1)]
        );
        assert!(store.get_pr(&PrKey::new(1, 5)).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(2, 1)).await.unwrap().is_none());
        assert_eq!(
            store.get_pr(&PrKey::new(3, 2)).await.unwrap(),
            Some(PrRecord {
                installation_id: 11,
                ..pr(3, 2, 10)
            })
        );
        let repositories = store
            .dashboard_summary(&PrFilter::default())
            .await
            .unwrap()
            .facets
            .repositories;
        assert_eq!(
            repositories
                .iter()
                .map(|facet| facet.repository.repository_id)
                .collect::<Vec<_>>(),
            vec![3],
            "only the other installation's repository survives"
        );
    }

    #[tokio::test]
    async fn a_prune_whose_result_is_lost_still_leaves_its_retirements_pending() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 4, 10)).await.unwrap();

        let pruned = store.retain_prs(1, &[], 20).await.unwrap();
        // Restate lost the step's result and runs it again: nothing is left to delete.
        let replayed = store.retain_prs(1, &[], 20).await.unwrap();

        assert_eq!(pruned, vec![PrKey::new(1, 1), PrKey::new(1, 4)]);
        assert_eq!(replayed, Vec::<PrKey>::new());
        let pending = store.pending_retirements().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|retirement| (retirement.key.clone(), retirement.synced_before))
                .collect::<Vec<_>>(),
            vec![(PrKey::new(1, 1), Some(20)), (PrKey::new(1, 4), Some(20))],
            "the keys the delete removed are queued with the fence the prune ran under"
        );
    }

    #[tokio::test]
    async fn acknowledging_retirements_keeps_the_ones_queued_since_the_read() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 2, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 3, 10)).await.unwrap();
        store.retain_prs(1, &[3], 20).await.unwrap();
        let read = store.pending_retirements().await.unwrap();
        // Another handler prunes between the read and its acknowledgement.
        store.retain_prs(1, &[], 30).await.unwrap();

        store
            .acknowledge_retirements(read.last().unwrap().id)
            .await
            .unwrap();
        // Restate runs the acknowledgement again: nothing else goes.
        store
            .acknowledge_retirements(read.last().unwrap().id)
            .await
            .unwrap();

        assert_eq!(
            read.iter()
                .map(|retirement| retirement.key.clone())
                .collect::<Vec<_>>(),
            vec![PrKey::new(1, 1), PrKey::new(1, 2)]
        );
        let pending = store.pending_retirements().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|retirement| (retirement.key.clone(), retirement.synced_before))
                .collect::<Vec<_>>(),
            vec![(PrKey::new(1, 3), Some(30))],
            "what was queued after the read waits for the next drain"
        );
        assert!(
            pending[0].id > read.last().unwrap().id,
            "ids only grow, which is what makes acknowledging through the last read safe"
        );
    }

    #[tokio::test]
    async fn a_repository_that_left_queues_its_pull_requests_under_the_sweeps_fence() {
        let (_directory, store) = test_store().await;
        store
            .replace_installation_repos(9, &[repo(1, 10), repo(2, 10)], 20)
            .await
            .unwrap();
        store.upsert_pr(&pr(1, 4, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(2, 1, 10)).await.unwrap();

        store
            .replace_installation_repos(9, &[repo(2, 30)], 25)
            .await
            .unwrap();

        let pending = store.pending_retirements().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|retirement| (retirement.key.clone(), retirement.synced_before))
                .collect::<Vec<_>>(),
            vec![(PrKey::new(1, 1), Some(25)), (PrKey::new(1, 4), Some(25))],
            "the closes for a repository that left carry the instant the listing started"
        );
    }

    #[tokio::test]
    async fn a_purge_queues_its_pull_requests_unfenced() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 5, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 8, 40)).await.unwrap();

        store.purge_installation(9).await.unwrap();

        let pending = store.pending_retirements().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|retirement| (retirement.key.clone(), retirement.synced_before))
                .collect::<Vec<_>>(),
            vec![(PrKey::new(1, 5), None), (PrKey::new(1, 8), None)],
            "the App has lost the installation, so nothing can reopen these"
        );
    }

    #[tokio::test]
    async fn retain_guards_concurrent_rows_and_repo_delete_cascades() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 2, 30)).await.unwrap();
        store.retain_prs(1, &[], 20).await.unwrap();
        assert!(store.get_pr(&PrKey::new(1, 1)).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(1, 2)).await.unwrap().is_some());
        store.purge_installation(9).await.unwrap();
        assert!(store.get_pr(&PrKey::new(1, 2)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn replacing_installation_repos_reports_the_pull_requests_of_repositories_that_left() {
        let (_directory, store) = test_store().await;
        store
            .replace_installation_repos(9, &[repo(1, 10), repo(2, 10), repo(3, 10)], 20)
            .await
            .unwrap();
        let other_installation = RepoRecord {
            installation_id: 11,
            ..repo(5, 10)
        };
        store.upsert_repo(&other_installation).await.unwrap();
        store.upsert_pr(&pr(1, 4, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(2, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(3, 2, 10)).await.unwrap();
        store.upsert_pr(&pr(5, 1, 10)).await.unwrap();

        let cascaded = store
            .replace_installation_repos(9, &[repo(2, 30)], 20)
            .await
            .unwrap();

        assert_eq!(
            cascaded,
            vec![PrKey::new(1, 1), PrKey::new(1, 4), PrKey::new(3, 2)],
            "every pull request of a repository that left, in key order"
        );
        assert!(store.get_repo(1).await.unwrap().is_none());
        assert!(store.get_repo(3).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(1, 1)).await.unwrap().is_none());
        assert!(store.get_pr(&PrKey::new(3, 2)).await.unwrap().is_none());
        assert_eq!(
            store.get_repo(2).await.unwrap(),
            Some(repo(2, 30)),
            "the repository that stayed is untouched"
        );
        assert!(store.get_pr(&PrKey::new(2, 1)).await.unwrap().is_some());
        assert_eq!(
            store.get_repo(5).await.unwrap(),
            Some(other_installation),
            "another installation's repository is none of this sweep's business"
        );
        assert!(store.get_pr(&PrKey::new(5, 1)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn replacing_installation_repos_spares_rows_synced_since_the_listing_started() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_repo(&repo(2, 30)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(2, 1, 10)).await.unwrap();

        // A listing that found no repositories at all: everything synced
        // before it started goes, and only that.
        let cascaded = store.replace_installation_repos(9, &[], 20).await.unwrap();

        assert_eq!(cascaded, vec![PrKey::new(1, 1)]);
        assert!(store.get_repo(1).await.unwrap().is_none());
        assert_eq!(
            store.get_repo(2).await.unwrap(),
            Some(repo(2, 30)),
            "a repository synced since the listing started was added behind the sweep's back"
        );
        assert!(store.get_pr(&PrKey::new(2, 1)).await.unwrap().is_some());
    }
}
