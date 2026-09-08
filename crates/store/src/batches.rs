//! The bulk-action audit tables — `batches`, `batch_targets` and
//! `running_batches` — as SQL and rows: what a read selects, in the order its
//! row mapper reads it back. The reads and writes themselves are the store's
//! trait methods, in `lib.rs`; their tests are here, with the tables.

use dependaboard_core::{BatchRecord, BatchTargetRecord, RunningBatch, UserId};
use libsql::Row;

use crate::{StoreError, stored_enum, unsigned};

/// The `batches` columns in the order [`batch_from_row`] reads them.
///
/// `installation_id` is nullable — a row from before 0011 the migration could
/// not attribute has none — and is read as if it were not: every read filters
/// on it, so a `NULL` row is never selected. A read that dropped the filter
/// would fail on such a row rather than return it.
pub(crate) fn select_batches_sql() -> &'static str {
    r#"SELECT batch_id, installation_id, action, requested_by, retried_from, started_at,
              completed_at, succeeded, rejected, failed
       FROM batches"#
}

/// The `batch_targets` columns in the order [`batch_target_from_row`] reads
/// them.
pub(crate) fn select_batch_targets_sql() -> &'static str {
    r#"SELECT batch_id, repository_id, owner, repo, number, title, html_url, head_sha, outcome
       FROM batch_targets"#
}

/// The `running_batches` columns in the order [`running_batch_from_row`]
/// reads them. `installation_id` is nullable and read as if it were not, as
/// on [`select_batches_sql`].
pub(crate) fn select_running_batches_sql() -> &'static str {
    r#"SELECT batch_id, installation_id, action, requested_by, retried_from, started_at,
              target_count
       FROM running_batches"#
}

/// A `batches` row, without its targets.
pub(crate) fn batch_from_row(row: Row) -> Result<BatchRecord, StoreError> {
    Ok(BatchRecord {
        batch_id: row.get(0)?,
        installation_id: unsigned(row.get::<i64>(1)?)?,
        action: stored_enum(row.get(2)?)?,
        requested_by: UserId::new(row.get::<String>(3)?),
        retried_from: row.get(4)?,
        started_at: unsigned(row.get::<i64>(5)?)?,
        completed_at: unsigned(row.get::<i64>(6)?)?,
        succeeded: unsigned(row.get::<i64>(7)?)?,
        rejected: unsigned(row.get::<i64>(8)?)?,
        failed: unsigned(row.get::<i64>(9)?)?,
        targets: Vec::new(),
    })
}

/// A `batch_targets` row as [`select_batch_targets_sql`] selects it: the
/// batch id in column 0, the target from column 1 on.
pub(crate) fn batch_target_from_row(row: Row) -> Result<BatchTargetRecord, StoreError> {
    Ok(BatchTargetRecord {
        repository_id: unsigned(row.get::<i64>(1)?)?,
        owner: row.get(2)?,
        repo: row.get(3)?,
        number: unsigned(row.get::<i64>(4)?)?,
        title: row.get(5)?,
        html_url: row.get(6)?,
        head_sha: row.get(7)?,
        outcome: serde_json::from_str(&row.get::<String>(8)?)?,
    })
}

/// A `running_batches` row.
pub(crate) fn running_batch_from_row(row: Row) -> Result<RunningBatch, StoreError> {
    Ok(RunningBatch {
        batch_id: row.get(0)?,
        installation_id: unsigned(row.get::<i64>(1)?)?,
        action: stored_enum(row.get(2)?)?,
        requested_by: UserId::new(row.get::<String>(3)?),
        retried_from: row.get(4)?,
        started_at: unsigned(row.get::<i64>(5)?)?,
        target_count: unsigned(row.get::<i64>(6)?)?,
    })
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{
        BulkActionKind, PrKey, ProjectedBatch, RejectReason, TargetOutcome, unix_seconds,
    };

    use crate::{
        ProjectionReader, ProjectionWriter,
        test_support::{pr, repo, test_store},
    };

    use super::*;

    /// A finished merge of two pull requests in `acme/repo-1`, asked for by `alice`
    /// as a retry of `batch-0`, with the first merged and the second rejected, run for
    /// the installation the fixtures' repositories belong to.
    fn batch(batch_id: &str, completed_at: u64) -> BatchRecord {
        BatchRecord {
            batch_id: batch_id.to_owned(),
            installation_id: 9,
            action: BulkActionKind::Merge,
            requested_by: UserId::new("alice"),
            retried_from: Some("batch-0".to_owned()),
            started_at: completed_at - 30,
            completed_at,
            succeeded: 1,
            rejected: 1,
            failed: 0,
            targets: vec![
                BatchTargetRecord {
                    repository_id: 1,
                    owner: "acme".to_owned(),
                    repo: "repo-1".to_owned(),
                    number: 1,
                    title: "Bump serde from 1.0.0 to 1.1.0".to_owned(),
                    html_url: "https://github.com/acme/repo-1/pull/1".to_owned(),
                    head_sha: Some("abc123".to_owned()),
                    outcome: TargetOutcome::Succeeded {
                        detail: "merged".to_owned(),
                        merge_sha: Some("9f8e7d6c5b4a".to_owned()),
                    },
                },
                BatchTargetRecord {
                    repository_id: 1,
                    owner: "acme".to_owned(),
                    repo: "repo-1".to_owned(),
                    number: 2,
                    title: "Bump tokio from 1.40.0 to 1.41.0".to_owned(),
                    html_url: "https://github.com/acme/repo-1/pull/2".to_owned(),
                    head_sha: Some("abc123".to_owned()),
                    outcome: TargetOutcome::Rejected {
                        reason: RejectReason::StaleSha {
                            expected: "abc123".to_owned(),
                            actual: "def456".to_owned(),
                        },
                    },
                },
            ],
        }
    }

    #[tokio::test]
    async fn finished_batches_are_kept_whole_and_listed_newest_first_up_to_the_limit() {
        let (_directory, store) = test_store().await;
        let older = batch("batch-older", 1_000);
        let newer = BatchRecord {
            action: BulkActionKind::Rebase,
            requested_by: UserId::new("bob"),
            retried_from: None,
            targets: vec![BatchTargetRecord {
                number: 5,
                html_url: "https://github.com/acme/repo-1/pull/5".to_owned(),
                outcome: TargetOutcome::Failed {
                    detail: "GitHub mutation failed with HTTP 500".to_owned(),
                },
                ..older.targets[0].clone()
            }],
            succeeded: 0,
            rejected: 0,
            failed: 1,
            ..batch("batch-newer", 2_000)
        };
        store.record_batch(&older).await.unwrap();
        store.record_batch(&newer).await.unwrap();

        assert_eq!(
            store.recent_batches(9, 10).await.unwrap(),
            vec![newer.clone(), older],
            "every column and every target comes back, targets in batch order: the batch \
             retried, each head, and each merge's commit included, and a batch that retried \
             nothing as such"
        );
        assert_eq!(store.recent_batches(9, 1).await.unwrap(), vec![newer]);
        assert_eq!(
            store.recent_batches(9, 0).await.unwrap(),
            Vec::<BatchRecord>::new()
        );
    }

    /// Restate may run the recording step again when the first attempt's result was
    /// lost, so recording the same batch twice must leave one record: the first.
    #[tokio::test]
    async fn recording_a_batch_again_keeps_the_first_record() {
        let (_directory, store) = test_store().await;
        let first = batch("batch-1", 1_000);
        store.record_batch(&first).await.unwrap();
        let again = BatchRecord {
            completed_at: 1_500,
            succeeded: 2,
            rejected: 0,
            targets: vec![first.targets[0].clone()],
            ..first.clone()
        };

        store.record_batch(&again).await.unwrap();

        assert_eq!(store.recent_batches(9, 10).await.unwrap(), vec![first]);
    }

    /// A merge of two pull requests `alice` asked for as a retry of `batch-0`, as
    /// the workflow writes it when it starts running the batch.
    fn running(batch_id: &str, started_at: u64) -> RunningBatch {
        RunningBatch {
            batch_id: batch_id.to_owned(),
            installation_id: 9,
            action: BulkActionKind::Merge,
            requested_by: UserId::new("alice"),
            retried_from: Some("batch-0".to_owned()),
            started_at,
            target_count: 2,
        }
    }

    /// The workflow writes a batch as running when it starts, so the audit
    /// view can list a batch that is still going.
    #[tokio::test]
    async fn started_batches_are_listed_as_running_newest_first() {
        let (_directory, store) = test_store().await;
        let older = running("batch-older", 1_000);
        let newer = RunningBatch {
            action: BulkActionKind::Rebase,
            requested_by: UserId::new("bob"),
            retried_from: None,
            target_count: 1,
            ..running("batch-newer", 2_000)
        };

        store.start_batch(&older).await.unwrap();
        store.start_batch(&newer).await.unwrap();

        assert_eq!(
            store.running_batches(9).await.unwrap(),
            vec![newer, older],
            "every column comes back, the batch retried included, newest first"
        );
    }

    /// The finished record takes the running listing away, so a batch is
    /// listed as running or as finished, never both.
    #[tokio::test]
    async fn recording_a_batch_stops_listing_it_as_running() {
        let (_directory, store) = test_store().await;
        let still_running = running("batch-newer", 2_000);
        store
            .start_batch(&running("batch-older", 1_000))
            .await
            .unwrap();
        store.start_batch(&still_running).await.unwrap();

        store
            .record_batch(&batch("batch-older", 3_000))
            .await
            .unwrap();

        assert_eq!(store.running_batches(9).await.unwrap(), vec![still_running]);
        assert_eq!(
            store.recent_batches(9, 10).await.unwrap(),
            vec![batch("batch-older", 3_000)]
        );
    }

    /// A workflow that ends without a finished batch — cancelled, or failed
    /// past what a target's own verdict can carry — has nothing to record,
    /// and must not stay listed as running for good.
    #[tokio::test]
    async fn unlisting_a_batch_takes_it_out_of_the_running_ones_and_records_nothing() {
        let (_directory, store) = test_store().await;
        store.start_batch(&running("batch-1", 1_000)).await.unwrap();

        store.unlist_batch("batch-1").await.unwrap();
        store.unlist_batch("never-listed").await.unwrap();

        assert_eq!(store.running_batches(9).await.unwrap(), Vec::new());
        assert_eq!(store.recent_batches(9, 10).await.unwrap(), Vec::new());
    }

    /// Restate may run the starting step again when the first attempt's result
    /// was lost; the batch is still one running batch.
    #[tokio::test]
    async fn starting_a_batch_again_leaves_it_listed_once() {
        let (_directory, store) = test_store().await;
        let first = running("batch-1", 1_000);
        store.start_batch(&first).await.unwrap();

        store
            .start_batch(&RunningBatch {
                started_at: 1_001,
                ..first.clone()
            })
            .await
            .unwrap();

        assert_eq!(store.running_batches(9).await.unwrap(), vec![first]);
    }

    /// The record is the audit trail, so it outlives what it is about: the pull
    /// requests it merged leave the projection, the workflow's state is cleared after
    /// its retention, and the batch is still there with its links.
    #[tokio::test]
    async fn a_recorded_batch_outlives_its_pull_requests_and_the_workflows_retention() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 2, 10)).await.unwrap();
        let thirty_days = 30 * 24 * 60 * 60;
        let long_ago = batch("batch-1", unix_seconds() - thirty_days);
        store.record_batch(&long_ago).await.unwrap();

        store.delete_pr(&PrKey::new(1, 1)).await.unwrap();
        store.purge_installation(9).await.unwrap();

        let listed = store.recent_batches(9, 10).await.unwrap();
        assert_eq!(listed, vec![long_ago]);
        assert_eq!(
            listed[0]
                .targets
                .iter()
                .map(|target| target.html_url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://github.com/acme/repo-1/pull/1",
                "https://github.com/acme/repo-1/pull/2"
            ]
        );
    }

    /// A dashboard handed a batch id alone — from a link — asks the projection
    /// what it holds of the batch before it asks Restate: the finished record,
    /// targets and all, for a batch that has run; the running listing for one
    /// still going; nothing for an id it has never heard of.
    #[tokio::test]
    async fn one_batch_is_read_back_by_id_as_finished_running_or_unknown() {
        let (_directory, store) = test_store().await;
        let finished = batch("batch-finished", 2_000);
        let still_running = running("batch-running", 3_000);
        store.record_batch(&finished).await.unwrap();
        store.start_batch(&still_running).await.unwrap();

        assert_eq!(
            store.get_batch(9, "batch-finished").await.unwrap(),
            Some(ProjectedBatch::Finished(finished)),
            "the record comes back whole, targets in batch order"
        );
        assert_eq!(
            store.get_batch(9, "batch-running").await.unwrap(),
            Some(ProjectedBatch::Running(still_running))
        );
        assert_eq!(store.get_batch(9, "batch-never-run").await.unwrap(), None);
    }

    /// Two deployments sharing one store each read their own batches: the listing and
    /// the by-id read are held to the installation asked for, and a batch of another
    /// installation is answered as one the projection has never heard of. A row the
    /// migration could not attribute is nobody's, and read by nobody.
    #[tokio::test]
    async fn batches_from_another_installation_are_not_read() {
        let (_directory, store) = test_store().await;
        let ours = batch("batch-ours", 2_000);
        let theirs = BatchRecord {
            installation_id: 10,
            ..batch("batch-theirs", 3_000)
        };
        let ours_running = running("batch-ours-running", 4_000);
        let theirs_running = RunningBatch {
            installation_id: 10,
            ..running("batch-theirs-running", 5_000)
        };
        store.record_batch(&ours).await.unwrap();
        store.record_batch(&theirs).await.unwrap();
        store.start_batch(&ours_running).await.unwrap();
        store.start_batch(&theirs_running).await.unwrap();
        let connection = store.connection().await;
        connection
            .execute_batch(
                r#"INSERT INTO batches (
                    batch_id, action, requested_by, started_at, completed_at,
                    succeeded, rejected, failed
                ) VALUES ('batch-nobodys', 'merge', 'alice', 970, 9000, 1, 0, 0);
                INSERT INTO running_batches (
                    batch_id, action, requested_by, started_at, target_count
                ) VALUES ('batch-nobodys-running', 'merge', 'alice', 9000, 2)"#,
            )
            .await
            .unwrap();
        drop(connection);

        assert_eq!(
            store.recent_batches(9, 10).await.unwrap(),
            vec![ours.clone()]
        );
        assert_eq!(
            store.recent_batches(10, 10).await.unwrap(),
            vec![theirs.clone()]
        );
        assert_eq!(
            store.recent_batches(9, 1).await.unwrap(),
            vec![ours.clone()],
            "the limit counts the installation's batches, not everyone's"
        );
        assert_eq!(
            store.running_batches(9).await.unwrap(),
            vec![ours_running.clone()]
        );
        assert_eq!(
            store.running_batches(10).await.unwrap(),
            vec![theirs_running.clone()]
        );
        assert_eq!(
            store.get_batch(9, "batch-ours").await.unwrap(),
            Some(ProjectedBatch::Finished(ours))
        );
        assert_eq!(
            store.get_batch(9, "batch-ours-running").await.unwrap(),
            Some(ProjectedBatch::Running(ours_running))
        );
        assert_eq!(store.get_batch(9, "batch-theirs").await.unwrap(), None);
        assert_eq!(
            store.get_batch(9, "batch-theirs-running").await.unwrap(),
            None
        );
        assert_eq!(store.get_batch(9, "batch-nobodys").await.unwrap(), None);
        assert_eq!(
            store.get_batch(9, "batch-nobodys-running").await.unwrap(),
            None
        );
    }

    /// The rows from before a batch named the batch it retried, a target kept the head
    /// it was sent against, and a merge kept the commit it made are still here, and read
    /// as batches that retried nothing anyone recorded, over heads nobody kept, with
    /// merges as what commit nobody knows — not as rows the store cannot read. The
    /// installation is the one the migration attributed them to; a row it could not
    /// attribute is nobody's to read, see `batches_from_another_installation_are_not_read`.
    #[tokio::test]
    async fn batches_recorded_before_the_link_the_heads_and_the_merge_commits_read_as_unknown() {
        let (_directory, store) = test_store().await;
        let connection = store.connection().await;
        connection
            .execute(
                r#"INSERT INTO batches (
                    batch_id, installation_id, action, requested_by, started_at, completed_at,
                    succeeded, rejected, failed
                ) VALUES ('batch-old', 9, 'merge', 'alice', 970, 1000, 1, 0, 0)"#,
                (),
            )
            .await
            .unwrap();
        connection
            .execute(
                r#"INSERT INTO batch_targets (
                    batch_id, position, repository_id, owner, repo, number, title, html_url, outcome
                ) VALUES (
                    'batch-old', 0, 1, 'acme', 'repo-1', 1, 'Bump serde',
                    'https://github.com/acme/repo-1/pull/1', '{"succeeded":{"detail":"merged"}}'
                )"#,
                (),
            )
            .await
            .unwrap();
        connection
            .execute(
                r#"INSERT INTO running_batches (
                    batch_id, installation_id, action, requested_by, started_at, target_count
                ) VALUES ('batch-old-running', 9, 'merge', 'alice', 2000, 2)"#,
                (),
            )
            .await
            .unwrap();
        drop(connection);

        let old = BatchRecord {
            batch_id: "batch-old".to_owned(),
            installation_id: 9,
            action: BulkActionKind::Merge,
            requested_by: UserId::new("alice"),
            retried_from: None,
            started_at: 970,
            completed_at: 1000,
            succeeded: 1,
            rejected: 0,
            failed: 0,
            targets: vec![BatchTargetRecord {
                repository_id: 1,
                owner: "acme".to_owned(),
                repo: "repo-1".to_owned(),
                number: 1,
                title: "Bump serde".to_owned(),
                html_url: "https://github.com/acme/repo-1/pull/1".to_owned(),
                head_sha: None,
                outcome: TargetOutcome::Succeeded {
                    detail: "merged".to_owned(),
                    merge_sha: None,
                },
            }],
        };
        assert_eq!(
            store.recent_batches(9, 10).await.unwrap(),
            vec![old.clone()]
        );
        assert_eq!(
            store.get_batch(9, "batch-old").await.unwrap(),
            Some(ProjectedBatch::Finished(old))
        );
        assert_eq!(
            store.running_batches(9).await.unwrap(),
            vec![RunningBatch {
                batch_id: "batch-old-running".to_owned(),
                installation_id: 9,
                action: BulkActionKind::Merge,
                requested_by: UserId::new("alice"),
                retried_from: None,
                started_at: 2000,
                target_count: 2,
            }]
        );
    }
}
