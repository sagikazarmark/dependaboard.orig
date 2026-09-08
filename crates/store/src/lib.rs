use std::{collections::BTreeMap, env, path::Path, str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use dependaboard_core::{
    BatchRecord, BatchTargetRecord, CursorError, DashboardPage, DashboardSummary, FacetCounts,
    LabelFacet, Mergeable, Page, PageCursor, PrFilter, PrKey, PrRecord, ProjectedBatch,
    ProjectionRevision, RepoFacet, RepoRecord, Retirement, RunningBatch, UserId, unix_seconds,
};
use libsql::{Builder, Row, Value};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::sync::{Mutex, MutexGuard};

mod filter;
mod migrations;

use filter::{Facet, filter_sql, without_facet};

#[derive(Clone, Debug)]
pub struct StoreConfig {
    pub url: String,
    /// Remote libSQL/Turso token. Empty for a local file; `Debug` redacts it.
    pub auth_token: SecretString,
}

impl StoreConfig {
    pub fn from_env() -> Self {
        Self {
            url: env::var("LIBSQL_URL").unwrap_or_else(|_| "data/dependaboard.db".to_owned()),
            auth_token: SecretString::from(env::var("LIBSQL_AUTH_TOKEN").unwrap_or_default()),
        }
    }

    pub fn local(path: impl AsRef<Path>) -> Self {
        Self {
            url: path.as_ref().to_string_lossy().into_owned(),
            auth_token: SecretString::from(String::new()),
        }
    }
}

/// One libSQL connection per store, shared by clones and serialised by a
/// mutex so a transaction on it never interleaves with another caller's
/// statements. SQLite has a single writer anyway, so little concurrency is
/// lost, and the busy timeout still waits out other processes. A future
/// dropped mid-transaction is safe: libSQL rolls the transaction back (or
/// closes the remote stream) when the `Transaction` drops.
#[derive(Clone)]
pub struct LibSqlPrStore {
    connection: Arc<Mutex<libsql::Connection>>,
}

impl LibSqlPrStore {
    pub async fn connect(config: &StoreConfig) -> Result<Self, StoreError> {
        let database = if config.url.starts_with("libsql://")
            || config.url.starts_with("https://")
            || config.url.starts_with("http://")
        {
            Builder::new_remote(
                config.url.clone(),
                config.auth_token.expose_secret().to_owned(),
            )
            .build()
            .await?
        } else {
            if let Some(parent) = Path::new(&config.url)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent)?;
            }
            Builder::new_local(&config.url).build().await?
        };
        let connection = database.connect()?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute("PRAGMA foreign_keys = ON", ()).await?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Applies any schema migrations the database has not seen yet.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let connection = self.connection().await;
        migrations::apply(&connection).await?;
        Ok(())
    }

    /// Holds the connection for the duration of one store operation. The
    /// lock is not re-entrant, so never call another `PrStore` method (or
    /// `migrate`) while a guard is alive.
    async fn connection(&self) -> MutexGuard<'_, libsql::Connection> {
        self.connection.lock().await
    }
}

#[async_trait]
pub trait PrStore: Send + Sync {
    async fn upsert_pr(&self, pr: &PrRecord) -> Result<(), StoreError>;
    async fn get_pr(&self, key: &PrKey) -> Result<Option<PrRecord>, StoreError>;
    async fn delete_pr(&self, key: &PrKey) -> Result<(), StoreError>;
    /// Reconciliation: drops this repository's rows that are not in `live` and
    /// were synced before the listing started, and reports which ones went.
    /// The `synced_before` guard keeps rows written by concurrent webhook
    /// syncs alive. The same keys are queued for retirement (see
    /// [`PrStore::pending_retirements`]) in the delete's own transaction, so
    /// the caller need not trust this call's return value to reach it.
    async fn retain_prs(
        &self,
        repository_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError>;
    /// One keyset page of the pull requests `filter` matches, newest update
    /// first, plus how many match in all.
    async fn list_prs(&self, filter: &PrFilter, page: Page) -> Result<DashboardPage, StoreError>;
    /// What frames the rows for `filter`: every facet's counts, each scoped to
    /// the filter minus its own dimension, and the read model's freshness. It
    /// does not depend on paging, so a caller moving to the next page need not
    /// ask again.
    async fn dashboard_summary(&self, filter: &PrFilter) -> Result<DashboardSummary, StoreError>;
    /// Two counters, one that moves whenever a row of the read model changes,
    /// however it changes, and one that moves only when a pull request row
    /// does. Cheap to read, so a dashboard can ask often and act only when
    /// an answer differs from the one it last saw.
    async fn projection_revision(&self) -> Result<ProjectionRevision, StoreError>;
    async fn prs_for_sha(&self, repository_id: u64, sha: &str)
    -> Result<Vec<PrRecord>, StoreError>;
    async fn upsert_repo(&self, repo: &RepoRecord) -> Result<(), StoreError>;
    async fn get_repo(&self, repository_id: u64) -> Result<Option<RepoRecord>, StoreError>;
    /// Installation reconciliation, in one transaction: upserts every repository in
    /// `repos`, then drops the installation's other repositories that were synced
    /// before the listing started, pull requests and all. Reports the pull requests
    /// that went with them; the same keys are queued for retirement under
    /// `synced_before` as their fence.
    async fn replace_installation_repos(
        &self,
        installation_id: u64,
        repos: &[RepoRecord],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError>;
    /// Drops the installation's repositories that are not in `live` and were synced
    /// before the listing started, pull requests and all, and reports which pull
    /// requests went; the same keys are queued for retirement under `synced_before`
    /// as their fence. The guard keeps repositories added by concurrent syncs alive.
    async fn retain_repos(
        &self,
        installation_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError>;
    /// Drops the installation's repositories and their pull requests, and
    /// reports which pull requests went; the same keys are queued for
    /// retirement with no fence, since the App has lost the installation and
    /// nothing can reopen them.
    async fn purge_installation(&self, installation_id: u64) -> Result<Vec<PrKey>, StoreError>;
    /// Every pull request a prune has removed and not yet acknowledged, oldest
    /// first. Reading is a step of its own in the sweep, so a prune whose
    /// result was lost is made good the next time anything drains.
    async fn pending_retirements(&self) -> Result<Vec<Retirement>, StoreError>;
    /// Forgets every retirement up to and including `through`, which a
    /// [`PrStore::pending_retirements`] read returned last. Ids only grow, so
    /// anything queued since that read stays for the next drain; acknowledging
    /// again is a no-op.
    async fn acknowledge_retirements(&self, through: u64) -> Result<(), StoreError>;
    /// Keeps a finished bulk action for the audit view, targets and all, and
    /// stops listing it as running. A batch already recorded is left as it
    /// was: Restate may run the recording step again when the first attempt's
    /// result was lost, and the first word is the one that stands.
    async fn record_batch(&self, batch: &BatchRecord) -> Result<(), StoreError>;
    /// The `limit` most recently finished batches of `installation_id`, newest
    /// first, each with every target's verdict in batch order. Another
    /// installation's batches are not counted against the limit, and a batch
    /// attributed to no installation is nobody's to read.
    async fn recent_batches(
        &self,
        installation_id: u64,
        limit: u32,
    ) -> Result<Vec<BatchRecord>, StoreError>;
    /// Lists a bulk action as running until [`PrStore::record_batch`] keeps
    /// it as finished, or [`PrStore::unlist_batch`] gives it up. A batch
    /// already listed is left as it was, for the same reason a recorded one
    /// is.
    async fn start_batch(&self, batch: &RunningBatch) -> Result<(), StoreError>;
    /// Stops listing a bulk action as running without recording it: the
    /// workflow ended without a finished batch to keep, as a cancelled one
    /// does. A batch not listed is left as it is.
    async fn unlist_batch(&self, batch_id: &str) -> Result<(), StoreError>;
    /// Every batch of `installation_id` started and not yet recorded or given
    /// up, newest first.
    async fn running_batches(&self, installation_id: u64) -> Result<Vec<RunningBatch>, StoreError>;
    /// What the projection holds of the batch `batch_id` names within
    /// `installation_id`: its finished record, targets and all, or its running
    /// listing; `None` for an id it has never heard of — and, just the same,
    /// for one it holds under another installation, so a foreign id is not
    /// told apart from an unknown one. One answer from one snapshot, so a
    /// batch finishing under the read is found as one or the other, not
    /// neither.
    async fn get_batch(
        &self,
        installation_id: u64,
        batch_id: &str,
    ) -> Result<Option<ProjectedBatch>, StoreError>;
}

#[async_trait]
impl PrStore for LibSqlPrStore {
    async fn upsert_pr(&self, pr: &PrRecord) -> Result<(), StoreError> {
        let connection = self.connection().await;
        let dependencies = serde_json::to_string(&pr.dependencies)?;
        let labels = serde_json::to_string(&pr.labels)?;
        connection
            .execute(
                r#"INSERT INTO pull_requests (
                    id, repository_id, owner, repo, number, title, html_url, dependency,
                    from_version, to_version, dependencies, update_type, head_sha,
                    check_status, mergeable, labels, created_at, updated_at, synced_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18, ?19
                ) ON CONFLICT(id) DO UPDATE SET
                    repository_id = excluded.repository_id,
                    owner = excluded.owner,
                    repo = excluded.repo,
                    number = excluded.number,
                    title = excluded.title,
                    html_url = excluded.html_url,
                    dependency = excluded.dependency,
                    from_version = excluded.from_version,
                    to_version = excluded.to_version,
                    dependencies = excluded.dependencies,
                    update_type = excluded.update_type,
                    head_sha = excluded.head_sha,
                    check_status = excluded.check_status,
                    mergeable = excluded.mergeable,
                    labels = excluded.labels,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    synced_at = excluded.synced_at"#,
                vec![
                    Value::Text(pr.id.clone()),
                    integer(pr.repository_id)?,
                    Value::Text(pr.owner.clone()),
                    Value::Text(pr.repo.clone()),
                    integer(pr.number)?,
                    Value::Text(pr.title.clone()),
                    Value::Text(pr.html_url.clone()),
                    option_text(pr.dependency.clone()),
                    option_text(pr.from_version.clone()),
                    option_text(pr.to_version.clone()),
                    Value::Text(dependencies),
                    Value::Text(pr.update_type.to_string()),
                    Value::Text(pr.head_sha.clone()),
                    Value::Text(pr.check_status.to_string()),
                    Value::Text(pr.mergeable.to_string()),
                    Value::Text(labels),
                    integer(pr.created_at)?,
                    integer(pr.updated_at)?,
                    integer(pr.synced_at)?,
                ],
            )
            .await?;
        Ok(())
    }

    async fn get_pr(&self, key: &PrKey) -> Result<Option<PrRecord>, StoreError> {
        let connection = self.connection().await;
        let mut rows = connection
            .query(
                &format!("{} WHERE p.id = ?1", select_pr_sql()),
                vec![Value::Text(key.to_string())],
            )
            .await?;
        rows.next().await?.map(pr_from_row).transpose()
    }

    async fn delete_pr(&self, key: &PrKey) -> Result<(), StoreError> {
        self.connection()
            .await
            .execute(
                "DELETE FROM pull_requests WHERE id = ?1",
                vec![Value::Text(key.to_string())],
            )
            .await?;
        Ok(())
    }

    async fn retain_prs(
        &self,
        repository_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError> {
        let connection = self.connection().await;
        let transaction = connection.transaction().await?;
        let pruned = retain_prs_on(&transaction, repository_id, live, synced_before).await?;
        transaction.commit().await?;
        Ok(pruned)
    }

    async fn list_prs(&self, filter: &PrFilter, page: Page) -> Result<DashboardPage, StoreError> {
        let connection = self.connection().await;
        // The count and the page are one answer, so they read one snapshot:
        // a sync landing between them must not leave a total that disagrees
        // with the rows.
        let transaction = connection.transaction().await?;
        let now = unix_seconds();
        let (where_sql, params) = filter_sql(filter, None, now)?;
        let total = scalar_u64(&transaction, &count_sql(&where_sql), params).await?;

        let cursor = page.after.as_deref().map(PageCursor::decode).transpose()?;
        let (page_where, mut page_params) = filter_sql(filter, cursor.as_ref(), now)?;
        let limit = page.normalized_limit() as usize;
        let limit_index = page_params.len() + 1;
        page_params.push(integer((limit + 1) as u64)?);
        let page_rows = transaction
            .query(&page_sql(&page_where, limit_index), page_params)
            .await?;
        let mut rows = collect_prs(page_rows).await?;
        transaction.commit().await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = has_more.then(|| rows.last()).flatten().map(|record| {
            PageCursor {
                updated_at: record.updated_at,
                id: record.id.clone(),
            }
            .encode()
        });

        Ok(DashboardPage {
            rows,
            total,
            next_cursor,
        })
    }

    async fn dashboard_summary(&self, filter: &PrFilter) -> Result<DashboardSummary, StoreError> {
        let connection = self.connection().await;
        // Every facet frames the same rows, so they all count one snapshot:
        // a sync landing between them must not leave facets that do not add
        // up to each other.
        let transaction = connection.transaction().await?;
        let now = unix_seconds();
        let facets = facet_counts(&transaction, filter, now).await?;
        let last_synced_at = scalar_optional_u64(
            &transaction,
            "SELECT MAX(synced_at) FROM pull_requests",
            Vec::new(),
        )
        .await?;
        transaction.commit().await?;
        Ok(DashboardSummary {
            facets,
            last_synced_at,
        })
    }

    async fn projection_revision(&self) -> Result<ProjectionRevision, StoreError> {
        let connection = self.connection().await;
        let row = single_row(
            &connection,
            "SELECT revision, pull_requests FROM projection_revision WHERE id = 1",
            Vec::new(),
        )
        .await?;
        Ok(ProjectionRevision {
            projection: unsigned(row.get::<i64>(0)?)?,
            pull_requests: unsigned(row.get::<i64>(1)?)?,
        })
    }

    async fn prs_for_sha(
        &self,
        repository_id: u64,
        sha: &str,
    ) -> Result<Vec<PrRecord>, StoreError> {
        let connection = self.connection().await;
        let rows = connection
            .query(
                &format!(
                    "{} WHERE p.repository_id = ?1 AND p.head_sha = ?2",
                    select_pr_sql()
                ),
                vec![integer(repository_id)?, Value::Text(sha.to_owned())],
            )
            .await?;
        collect_prs(rows).await
    }

    async fn upsert_repo(&self, repo: &RepoRecord) -> Result<(), StoreError> {
        let connection = self.connection().await;
        upsert_repo_on(&connection, repo).await
    }

    async fn get_repo(&self, repository_id: u64) -> Result<Option<RepoRecord>, StoreError> {
        let connection = self.connection().await;
        let mut rows = connection
            .query(
                &format!("{} WHERE repository_id = ?1", select_repo_sql()),
                vec![integer(repository_id)?],
            )
            .await?;
        rows.next().await?.map(repo_from_row).transpose()
    }

    async fn replace_installation_repos(
        &self,
        installation_id: u64,
        repos: &[RepoRecord],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError> {
        let connection = self.connection().await;
        let transaction = connection.transaction().await?;
        for repo in repos {
            upsert_repo_on(&transaction, repo).await?;
        }
        let live = repos
            .iter()
            .map(|repo| repo.repository_id)
            .collect::<Vec<_>>();
        let cascaded = retain_repos_on(&transaction, installation_id, &live, synced_before).await?;
        transaction.commit().await?;
        Ok(cascaded)
    }

    async fn retain_repos(
        &self,
        installation_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError> {
        let connection = self.connection().await;
        let transaction = connection.transaction().await?;
        let cascaded = retain_repos_on(&transaction, installation_id, live, synced_before).await?;
        transaction.commit().await?;
        Ok(cascaded)
    }

    async fn purge_installation(&self, installation_id: u64) -> Result<Vec<PrKey>, StoreError> {
        let connection = self.connection().await;
        let transaction = connection.transaction().await?;
        let purged = delete_repositories_where(
            &transaction,
            "installation_id = ?1",
            vec![integer(installation_id)?],
            None,
        )
        .await?;
        transaction.commit().await?;
        Ok(purged)
    }

    async fn pending_retirements(&self) -> Result<Vec<Retirement>, StoreError> {
        let connection = self.connection().await;
        let mut rows = connection
            .query(
                "SELECT id, repository_id, number, synced_before
                 FROM pull_request_retirements ORDER BY id",
                (),
            )
            .await?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next().await? {
            pending.push(Retirement {
                id: unsigned(row.get::<i64>(0)?)?,
                key: PrKey::new(unsigned(row.get::<i64>(1)?)?, unsigned(row.get::<i64>(2)?)?),
                synced_before: row.get::<Option<i64>>(3)?.map(unsigned).transpose()?,
            });
        }
        Ok(pending)
    }

    async fn acknowledge_retirements(&self, through: u64) -> Result<(), StoreError> {
        self.connection()
            .await
            .execute(
                "DELETE FROM pull_request_retirements WHERE id <= ?1",
                vec![integer(through)?],
            )
            .await?;
        Ok(())
    }

    async fn record_batch(&self, batch: &BatchRecord) -> Result<(), StoreError> {
        let connection = self.connection().await;
        let transaction = connection.transaction().await?;
        // Whether or not this is the record that stands, the batch has run.
        transaction
            .execute(
                "DELETE FROM running_batches WHERE batch_id = ?1",
                vec![Value::Text(batch.batch_id.clone())],
            )
            .await?;
        let inserted = transaction
            .execute(
                r#"INSERT INTO batches (
                    batch_id, installation_id, action, requested_by, retried_from, started_at,
                    completed_at, succeeded, rejected, failed
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ON CONFLICT(batch_id) DO NOTHING"#,
                vec![
                    Value::Text(batch.batch_id.clone()),
                    integer(batch.installation_id)?,
                    Value::Text(batch.action.to_string()),
                    Value::Text(batch.requested_by.to_string()),
                    option_text(batch.retried_from.clone()),
                    integer(batch.started_at)?,
                    integer(batch.completed_at)?,
                    integer(batch.succeeded)?,
                    integer(batch.rejected)?,
                    integer(batch.failed)?,
                ],
            )
            .await?;
        if inserted == 0 {
            transaction.commit().await?;
            return Ok(());
        }
        for (position, target) in batch.targets.iter().enumerate() {
            transaction
                .execute(
                    r#"INSERT INTO batch_targets (
                        batch_id, position, repository_id, owner, repo, number,
                        title, html_url, head_sha, outcome
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
                    vec![
                        Value::Text(batch.batch_id.clone()),
                        integer(position as u64)?,
                        integer(target.repository_id)?,
                        Value::Text(target.owner.clone()),
                        Value::Text(target.repo.clone()),
                        integer(target.number)?,
                        Value::Text(target.title.clone()),
                        Value::Text(target.html_url.clone()),
                        option_text(target.head_sha.clone()),
                        Value::Text(serde_json::to_string(&target.outcome)?),
                    ],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn recent_batches(
        &self,
        installation_id: u64,
        limit: u32,
    ) -> Result<Vec<BatchRecord>, StoreError> {
        let connection = self.connection().await;
        let mut rows = connection
            .query(
                &format!(
                    "{} WHERE installation_id = ?1 ORDER BY completed_at DESC, batch_id DESC LIMIT ?2",
                    select_batches_sql()
                ),
                vec![integer(installation_id)?, Value::Integer(i64::from(limit))],
            )
            .await?;
        let mut batches = Vec::new();
        while let Some(row) = rows.next().await? {
            batches.push(batch_from_row(row)?);
        }
        if batches.is_empty() {
            return Ok(batches);
        }
        let placeholders = (1..=batches.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut targets = connection
            .query(
                &format!(
                    "{} WHERE batch_id IN ({placeholders}) ORDER BY batch_id, position",
                    select_batch_targets_sql()
                ),
                batches
                    .iter()
                    .map(|batch| Value::Text(batch.batch_id.clone()))
                    .collect::<Vec<_>>(),
            )
            .await?;
        let mut by_batch: BTreeMap<String, Vec<BatchTargetRecord>> = BTreeMap::new();
        while let Some(row) = targets.next().await? {
            by_batch
                .entry(row.get(0)?)
                .or_default()
                .push(batch_target_from_row(row)?);
        }
        for batch in &mut batches {
            batch.targets = by_batch.remove(&batch.batch_id).unwrap_or_default();
        }
        Ok(batches)
    }

    async fn start_batch(&self, batch: &RunningBatch) -> Result<(), StoreError> {
        self.connection()
            .await
            .execute(
                r#"INSERT INTO running_batches (
                    batch_id, installation_id, action, requested_by, retried_from, started_at,
                    target_count
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(batch_id) DO NOTHING"#,
                vec![
                    Value::Text(batch.batch_id.clone()),
                    integer(batch.installation_id)?,
                    Value::Text(batch.action.to_string()),
                    Value::Text(batch.requested_by.to_string()),
                    option_text(batch.retried_from.clone()),
                    integer(batch.started_at)?,
                    integer(batch.target_count)?,
                ],
            )
            .await?;
        Ok(())
    }

    async fn unlist_batch(&self, batch_id: &str) -> Result<(), StoreError> {
        self.connection()
            .await
            .execute(
                "DELETE FROM running_batches WHERE batch_id = ?1",
                vec![Value::Text(batch_id.to_owned())],
            )
            .await?;
        Ok(())
    }

    async fn running_batches(&self, installation_id: u64) -> Result<Vec<RunningBatch>, StoreError> {
        let mut rows = self
            .connection()
            .await
            .query(
                &format!(
                    "{} WHERE installation_id = ?1 ORDER BY started_at DESC, batch_id DESC",
                    select_running_batches_sql()
                ),
                vec![integer(installation_id)?],
            )
            .await?;
        let mut batches = Vec::new();
        while let Some(row) = rows.next().await? {
            batches.push(running_batch_from_row(row)?);
        }
        Ok(batches)
    }

    async fn get_batch(
        &self,
        installation_id: u64,
        batch_id: &str,
    ) -> Result<Option<ProjectedBatch>, StoreError> {
        let connection = self.connection().await;
        // The record and the listing are one answer, so they read one
        // snapshot: a batch finishing between the two reads must not be found
        // as neither.
        let transaction = connection.transaction().await?;
        let key = vec![Value::Text(batch_id.to_owned())];
        let scoped_key = vec![Value::Text(batch_id.to_owned()), integer(installation_id)?];
        let recorded = transaction
            .query(
                &format!(
                    "{} WHERE batch_id = ?1 AND installation_id = ?2",
                    select_batches_sql()
                ),
                scoped_key.clone(),
            )
            .await?
            .next()
            .await?
            .map(batch_from_row)
            .transpose()?;
        let projected = match recorded {
            Some(mut batch) => {
                let mut targets = transaction
                    .query(
                        &format!(
                            "{} WHERE batch_id = ?1 ORDER BY position",
                            select_batch_targets_sql()
                        ),
                        key,
                    )
                    .await?;
                while let Some(row) = targets.next().await? {
                    batch.targets.push(batch_target_from_row(row)?);
                }
                Some(ProjectedBatch::Finished(batch))
            }
            None => transaction
                .query(
                    &format!(
                        "{} WHERE batch_id = ?1 AND installation_id = ?2",
                        select_running_batches_sql()
                    ),
                    scoped_key,
                )
                .await?
                .next()
                .await?
                .map(running_batch_from_row)
                .transpose()?
                .map(ProjectedBatch::Running),
        };
        transaction.commit().await?;
        Ok(projected)
    }
}

/// The `batches` columns in the order [`batch_from_row`] reads them.
///
/// `installation_id` is nullable — a row from before 0011 the migration could
/// not attribute has none — and is read as if it were not: every read filters
/// on it, so a `NULL` row is never selected. A read that dropped the filter
/// would fail on such a row rather than return it.
fn select_batches_sql() -> &'static str {
    r#"SELECT batch_id, installation_id, action, requested_by, retried_from, started_at,
              completed_at, succeeded, rejected, failed
       FROM batches"#
}

/// The `batch_targets` columns in the order [`batch_target_from_row`] reads
/// them.
fn select_batch_targets_sql() -> &'static str {
    r#"SELECT batch_id, repository_id, owner, repo, number, title, html_url, head_sha, outcome
       FROM batch_targets"#
}

/// The `running_batches` columns in the order [`running_batch_from_row`]
/// reads them. `installation_id` is nullable and read as if it were not, as
/// on [`select_batches_sql`].
fn select_running_batches_sql() -> &'static str {
    r#"SELECT batch_id, installation_id, action, requested_by, retried_from, started_at,
              target_count
       FROM running_batches"#
}

/// A `batches` row, without its targets.
fn batch_from_row(row: Row) -> Result<BatchRecord, StoreError> {
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
fn batch_target_from_row(row: Row) -> Result<BatchTargetRecord, StoreError> {
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
fn running_batch_from_row(row: Row) -> Result<RunningBatch, StoreError> {
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

fn select_pr_sql() -> &'static str {
    r#"SELECT
        p.id, p.repository_id, r.installation_id, p.owner, p.repo, p.number,
        p.title, p.html_url, p.dependency, p.from_version, p.to_version,
        p.dependencies, p.update_type, p.head_sha, p.check_status, p.mergeable,
        p.labels, p.created_at, p.updated_at, p.synced_at
       FROM pull_requests p
       JOIN repositories r ON r.repository_id = p.repository_id"#
}

/// The dashboard's total for a `WHERE` clause from [`filter_sql`].
fn count_sql(where_sql: &str) -> String {
    format!("SELECT COUNT(*) FROM pull_requests p {where_sql}")
}

/// One keyset page for a `WHERE` clause from [`filter_sql`]; the limit binds
/// as parameter `limit_index`.
fn page_sql(where_sql: &str, limit_index: usize) -> String {
    format!(
        "{} {where_sql} ORDER BY p.updated_at DESC, p.id DESC LIMIT ?{limit_index}",
        select_pr_sql()
    )
}

fn pr_from_row(row: Row) -> Result<PrRecord, StoreError> {
    Ok(PrRecord {
        id: row.get(0)?,
        repository_id: unsigned(row.get::<i64>(1)?)?,
        installation_id: unsigned(row.get::<i64>(2)?)?,
        owner: row.get(3)?,
        repo: row.get(4)?,
        number: unsigned(row.get::<i64>(5)?)?,
        title: row.get(6)?,
        html_url: row.get(7)?,
        dependency: row.get(8)?,
        from_version: row.get(9)?,
        to_version: row.get(10)?,
        dependencies: serde_json::from_str(&row.get::<String>(11)?)?,
        update_type: stored_enum(row.get(12)?)?,
        head_sha: row.get(13)?,
        check_status: stored_enum(row.get(14)?)?,
        // The column is nullable and was once written verbatim from GitHub, so
        // NULL and any unrecognised legacy text deliberately fold into Unknown
        // rather than surfacing as CorruptEnum.
        mergeable: row
            .get::<Option<String>>(15)?
            .as_deref()
            .map_or(Mergeable::Unknown, Mergeable::from_github_state),
        labels: serde_json::from_str(&row.get::<String>(16)?)?,
        created_at: unsigned(row.get::<i64>(17)?)?,
        updated_at: unsigned(row.get::<i64>(18)?)?,
        synced_at: unsigned(row.get::<i64>(19)?)?,
    })
}

/// Drains the rows of a [`select_pr_sql`] query, in the order the database
/// produced them. Taking the rows by value means nothing of the statement
/// outlives the call, so a caller can end its transaction right after.
async fn collect_prs(mut rows: libsql::Rows) -> Result<Vec<PrRecord>, StoreError> {
    let mut records = Vec::new();
    while let Some(row) = rows.next().await? {
        records.push(pr_from_row(row)?);
    }
    Ok(records)
}

/// Parses a column that persists an enum's `Display` form. Unrecognised text
/// is corrupt data.
fn stored_enum<T: FromStr>(value: String) -> Result<T, StoreError> {
    T::from_str(&value).map_err(|_| StoreError::CorruptEnum(value))
}

fn select_repo_sql() -> &'static str {
    "SELECT repository_id, installation_id, owner, repo, merge_method, synced_at FROM repositories"
}

fn repo_from_row(row: Row) -> Result<RepoRecord, StoreError> {
    Ok(RepoRecord {
        repository_id: unsigned(row.get::<i64>(0)?)?,
        installation_id: unsigned(row.get::<i64>(1)?)?,
        owner: row.get(2)?,
        repo: row.get(3)?,
        merge_method: row.get::<Option<String>>(4)?.map(stored_enum).transpose()?,
        synced_at: unsigned(row.get::<i64>(5)?)?,
    })
}

async fn facet_counts(
    connection: &libsql::Connection,
    filter: &PrFilter,
    now: u64,
) -> Result<FacetCounts, StoreError> {
    let (checks_where, checks_params) = facet_scope(filter, Facet::Checks, now)?;
    let checks = enum_counts(
        connection,
        &format!(
            "SELECT p.check_status, COUNT(*) FROM pull_requests p {checks_where} GROUP BY p.check_status"
        ),
        checks_params,
    )
    .await?;

    let (types_where, types_params) = facet_scope(filter, Facet::UpdateTypes, now)?;
    let update_types = enum_counts(
        connection,
        &format!(
            "SELECT p.update_type, COUNT(*) FROM pull_requests p {types_where} GROUP BY p.update_type"
        ),
        types_params,
    )
    .await?;

    // Ties fall back to case-insensitive name order; the GROUP BY stays
    // exact because the label filter matches labels byte for byte.
    let (labels_where, labels_params) = facet_scope(filter, Facet::Labels, now)?;
    let labels = grouped_counts(
        connection,
        &format!(
            "SELECT lbl.value, COUNT(*) FROM pull_requests p, json_each(p.labels) lbl {labels_where} GROUP BY lbl.value ORDER BY COUNT(*) DESC, lbl.value COLLATE NOCASE"
        ),
        labels_params,
    )
    .await?
    .into_iter()
    .map(|(label, count)| LabelFacet { label, count })
    .collect();

    let (repos_where, repos_params) = facet_scope(filter, Facet::Repositories, now)?;
    let repositories = repository_facets(connection, &repos_where, repos_params).await?;

    Ok(FacetCounts {
        checks,
        update_types,
        labels,
        repositories,
    })
}

/// The `WHERE` clause a facet counts within: `filter` minus the facet's own
/// dimension, as [`filter_sql`] renders it.
fn facet_scope(
    filter: &PrFilter,
    facet: Facet,
    now: u64,
) -> Result<(String, Vec<Value>), StoreError> {
    filter_sql(&without_facet(filter, facet), None, now)
}

/// Every repository, with how many of its pull requests satisfy `where_sql`
/// (a clause from [`filter_sql`] over `pull_requests p`), in owner then name
/// order, case-insensitively, so consecutive entries share an owner. The
/// clause is used as is, on a grouped subquery, so its shape stays
/// [`filter_sql`]'s business.
async fn repository_facets(
    connection: &libsql::Connection,
    where_sql: &str,
    params: Vec<Value>,
) -> Result<Vec<RepoFacet>, StoreError> {
    let mut rows = connection
        .query(
            &format!(
                "SELECT r.repository_id, r.installation_id, r.owner, r.repo, r.merge_method, r.synced_at, \
                 COALESCE(c.matching, 0) \
                 FROM repositories r \
                 LEFT JOIN (SELECT p.repository_id, COUNT(*) AS matching FROM pull_requests p {where_sql} GROUP BY p.repository_id) c \
                 ON c.repository_id = r.repository_id \
                 ORDER BY r.owner COLLATE NOCASE, r.repo COLLATE NOCASE"
            ),
            params,
        )
        .await?;
    let mut facets = Vec::new();
    while let Some(row) = rows.next().await? {
        let count = unsigned(row.get::<i64>(6)?)?;
        facets.push(RepoFacet {
            repository: repo_from_row(row)?,
            count,
        });
    }
    Ok(facets)
}

/// Groups by a column that persists an enum's `Display` form and keys the
/// result by the parsed enum, so callers never see the raw text. Unrecognised
/// text is corrupt data, exactly as it is when reading a row.
async fn enum_counts<T>(
    connection: &libsql::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<BTreeMap<T, u64>, StoreError>
where
    T: FromStr + Ord,
{
    grouped_counts(connection, sql, params)
        .await?
        .into_iter()
        .map(|(key, count)| Ok((stored_enum(key)?, count)))
        .collect()
}

/// Runs a `SELECT key, COUNT(*)` query and returns the rows in the order the
/// database produced them, so an `ORDER BY` in the query survives.
async fn grouped_counts(
    connection: &libsql::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<(String, u64)>, StoreError> {
    let mut rows = connection.query(sql, params).await?;
    let mut counts = Vec::new();
    while let Some(row) = rows.next().await? {
        counts.push((row.get(0)?, unsigned(row.get::<i64>(1)?)?));
    }
    Ok(counts)
}

/// The one row a query is bound to produce — an aggregate's, or the
/// single-row table's — which not coming back is [`StoreError::MissingScalar`].
async fn single_row(
    connection: &libsql::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Row, StoreError> {
    let mut rows = connection.query(sql, params).await?;
    rows.next().await?.ok_or(StoreError::MissingScalar)
}

async fn scalar_u64(
    connection: &libsql::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<u64, StoreError> {
    let row = single_row(connection, sql, params).await?;
    unsigned(row.get::<i64>(0)?)
}

async fn scalar_optional_u64(
    connection: &libsql::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Option<u64>, StoreError> {
    let row = single_row(connection, sql, params).await?;
    row.get::<Option<i64>>(0)?.map(unsigned).transpose()
}

async fn upsert_repo_on(
    connection: &libsql::Connection,
    repo: &RepoRecord,
) -> Result<(), StoreError> {
    connection
        .execute(
            r#"INSERT INTO repositories (
                repository_id, installation_id, owner, repo, merge_method, synced_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(repository_id) DO UPDATE SET
                installation_id = excluded.installation_id,
                owner = excluded.owner,
                repo = excluded.repo,
                merge_method = excluded.merge_method,
                synced_at = excluded.synced_at"#,
            vec![
                integer(repo.repository_id)?,
                integer(repo.installation_id)?,
                Value::Text(repo.owner.clone()),
                Value::Text(repo.repo.clone()),
                option_text(repo.merge_method.map(|method| method.to_string())),
                integer(repo.synced_at)?,
            ],
        )
        .await?;
    Ok(())
}

/// Prunes one repository's pull requests down to `live`, reporting the keys it removed
/// and queueing them for retirement under `synced_before`. Two statements, so run it
/// inside a transaction: the queue must land with the delete or not at all.
async fn retain_prs_on(
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
async fn retain_repos_on(
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
async fn delete_repositories_where(
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

fn option_text(value: Option<String>) -> Value {
    value.map(Value::Text).unwrap_or(Value::Null)
}

fn integer(value: u64) -> Result<Value, StoreError> {
    Ok(Value::Integer(
        i64::try_from(value).map_err(|_| StoreError::IntegerOverflow)?,
    ))
}

fn unsigned(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::IntegerOverflow)
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("local database directory could not be created: {0}")]
    Io(#[from] std::io::Error),
    #[error("libSQL operation failed: {0}")]
    Database(#[from] libsql::Error),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Cursor(#[from] CursorError),
    #[error("stored enum value is invalid: {0}")]
    CorruptEnum(String),
    #[error("database integer is out of range")]
    IntegerOverflow,
    #[error("aggregate query did not return a row")]
    MissingScalar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreErrorClass {
    Retryable,
    Terminal,
}

impl StoreError {
    pub fn class(&self) -> StoreErrorClass {
        match self {
            Self::Database(error) if retryable_database_error(error) => StoreErrorClass::Retryable,
            Self::Io(_)
            | Self::Database(_)
            | Self::Json(_)
            | Self::Cursor(_)
            | Self::CorruptEnum(_)
            | Self::IntegerOverflow
            | Self::MissingScalar => StoreErrorClass::Terminal,
        }
    }
}

fn retryable_database_error(error: &libsql::Error) -> bool {
    match error {
        libsql::Error::SqliteFailure(code, _) => retryable_sqlite_code(*code),
        // Only the embedded-replica connection reports this way; a store built
        // with `Builder::new_remote` never does.
        libsql::Error::RemoteSqliteFailure(code, extended_code, _) => {
            retryable_sqlite_code(*code) || retryable_sqlite_code(*extended_code)
        }
        libsql::Error::ConnectionFailed(_) | libsql::Error::WalConflict => true,
        // Every failure a store built with `Builder::new_remote` reports — a
        // transport error, a dropped stream, an HTTP 5xx or 429, and even a
        // server-side SQLITE_BUSY or constraint violation — arrives here, as a
        // boxed `HranaError` whose module libsql (0.9) keeps private, so it can
        // be neither matched nor downcast. Treat the whole variant as
        // transient: every caller that consults the class has a bounded retry,
        // so a request that is genuinely bad costs at most its budget before
        // the same terminal report, while the alternative fails every blip on
        // the first attempt. Revisit if libsql exports the type or a
        // structured code.
        libsql::Error::Hrana(_) => true,
        _ => false,
    }
}

fn retryable_sqlite_code(code: i32) -> bool {
    // Extended SQLite result codes retain the primary result in the low byte.
    // Contention (BUSY, LOCKED, IOERR) clears on its own. Of the constraint
    // codes only the foreign key one does: a pull request can race the sync
    // that inserts its repository. A primary key or unique violation is a
    // programming error, and retrying it would only delay the report.
    matches!(code & 0xff, 5 | 6 | 10) || code == 787 // SQLITE_CONSTRAINT_FOREIGNKEY
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{
        BatchRecord, BatchTargetRecord, BulkActionKind, CheckStatus, DependencyUpdate, MergeMethod,
        RejectReason, RunningBatch, TargetOutcome, UpdateType, UserId,
    };
    use tempfile::TempDir;

    use super::*;

    async fn test_store() -> (TempDir, LibSqlPrStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = LibSqlPrStore::connect(&StoreConfig::local(database_path(&directory)))
            .await
            .unwrap();
        (directory, store)
    }

    fn database_path(directory: &TempDir) -> std::path::PathBuf {
        directory.path().join("db.sqlite")
    }

    /// An independent connection to the test database, standing in for the
    /// other process (web or Restate) that shares the file in production.
    async fn sidecar(directory: &TempDir) -> libsql::Connection {
        Builder::new_local(database_path(directory))
            .build()
            .await
            .unwrap()
            .connect()
            .unwrap()
    }

    fn repo(id: u64, synced_at: u64) -> RepoRecord {
        RepoRecord {
            repository_id: id,
            installation_id: 9,
            owner: "acme".to_owned(),
            repo: format!("repo-{id}"),
            merge_method: None,
            synced_at,
        }
    }

    fn pr(repository_id: u64, number: u64, synced_at: u64) -> PrRecord {
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

    #[tokio::test]
    async fn an_in_memory_store_keeps_its_schema_across_calls() {
        // SQLite gives every new connection to `:memory:` a fresh, empty
        // database, so this only works if the store reuses one connection.
        let store = LibSqlPrStore::connect(&StoreConfig::local(":memory:"))
            .await
            .unwrap();

        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();

        assert_eq!(
            store.get_pr(&PrKey::new(1, 1)).await.unwrap(),
            Some(pr(1, 1, 10))
        );
    }

    #[test]
    fn config_debug_output_redacts_the_auth_token() {
        let config = StoreConfig {
            url: "libsql://dependaboard.turso.io".to_owned(),
            auth_token: SecretString::from("hunter2"),
        };

        let debug = format!("{config:?}");

        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("libsql://dependaboard.turso.io"), "{debug}");
    }

    #[tokio::test]
    async fn upsert_and_keyset_page_round_trip() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        for number in 1..=3 {
            store.upsert_pr(&pr(1, number, 10)).await.unwrap();
        }
        let first = store
            .list_prs(
                &PrFilter::default(),
                Page {
                    limit: 2,
                    after: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(first.total, 3);
        assert_eq!(
            first.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [3, 2]
        );
        let second = store
            .list_prs(
                &PrFilter::default(),
                Page {
                    limit: 2,
                    after: first.next_cursor,
                },
            )
            .await
            .unwrap();
        assert_eq!(second.rows[0].number, 1);
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn keyset_pages_break_equal_updated_at_ties_on_id_without_gaps_or_repeats() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        // Numbers straddle a digit boundary because ids are text: "1#9" sorts
        // after "1#11", so ORDER BY and the cursor predicate must agree on
        // the same (text) comparison or a page boundary skips a row.
        for number in 8..=11 {
            let mut record = pr(1, number, 10);
            record.updated_at = 500;
            store.upsert_pr(&record).await.unwrap();
        }
        let unpaged = store
            .list_prs(&PrFilter::default(), Page::default())
            .await
            .unwrap();
        assert_eq!(
            unpaged.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [9, 8, 11, 10]
        );

        let mut walked = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .list_prs(&PrFilter::default(), Page { limit: 1, after })
                .await
                .unwrap();
            walked.extend(page.rows.iter().map(|pr| pr.number));
            after = page.next_cursor;
            if after.is_none() {
                break;
            }
            assert!(walked.len() < 8, "cursor never terminated: {walked:?}");
        }

        assert_eq!(walked, [9, 8, 11, 10]);
    }

    #[tokio::test]
    async fn searching_for_like_wildcards_matches_them_literally() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        let titles = [
            (1, "Bump coverage to 100%"),
            (2, "Bump coverage to 100"),
            (3, "Bump snake_case helper"),
        ];
        for (number, title) in titles {
            let mut record = pr(1, number, 10);
            record.title = title.to_owned();
            store.upsert_pr(&record).await.unwrap();
        }

        for (query, expected) in [("%", [1]), ("_", [3])] {
            let result = store
                .list_prs(
                    &PrFilter {
                        query: Some(query.to_owned()),
                        ..Default::default()
                    },
                    Page::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                result.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
                expected,
                "query {query:?}"
            );
        }
    }

    #[tokio::test]
    async fn grouped_dependency_and_all_labels_are_filterable() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        let mut record = pr(1, 1, 10);
        record.dependency = None;
        record.dependencies.push(DependencyUpdate {
            name: "tokio".to_owned(),
            from_version: None,
            to_version: None,
            update_type: UpdateType::Patch,
        });
        store.upsert_pr(&record).await.unwrap();
        let result = store
            .list_prs(
                &PrFilter {
                    dependency: Some("tokio".to_owned()),
                    labels: vec!["dependencies".to_owned(), "rust".to_owned()],
                    ..Default::default()
                },
                Page::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.total, 1);
    }

    fn grouped(repository_id: u64, number: u64, names: &[&str]) -> PrRecord {
        let mut record = pr(repository_id, number, 10);
        record.dependency = None;
        record.dependencies = names
            .iter()
            .map(|name| DependencyUpdate {
                name: (*name).to_owned(),
                from_version: None,
                to_version: None,
                update_type: UpdateType::Patch,
            })
            .collect();
        record
    }

    #[tokio::test]
    async fn dependency_filter_ignores_case_for_single_and_grouped_updates() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        let mut single = pr(1, 1, 10);
        single.dependency = Some("Serde".to_owned());
        store.upsert_pr(&single).await.unwrap();
        store
            .upsert_pr(&grouped(1, 2, &["Tokio", "hyper"]))
            .await
            .unwrap();
        let mut other = pr(1, 3, 10);
        other.dependency = Some("HYPER".to_owned());
        store.upsert_pr(&other).await.unwrap();

        for (dependency, expected) in [
            ("serde", vec![1]),
            ("TOKIO", vec![2]),
            ("Hyper", vec![3, 2]),
        ] {
            let result = store
                .list_prs(
                    &PrFilter {
                        dependency: Some(dependency.to_owned()),
                        ..Default::default()
                    },
                    Page::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                result.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
                expected,
                "dependency {dependency:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_dependency_filter_is_served_by_its_index() {
        let (_directory, store) = test_store().await;
        let filter = PrFilter {
            dependency: Some("serde".to_owned()),
            ..Default::default()
        };
        let (where_sql, _) = filter_sql(&filter, None, unix_seconds()).unwrap();
        let connection = store.connection().await;

        for sql in [count_sql(&where_sql), page_sql(&where_sql, 2)] {
            let mut rows = connection
                .query(&format!("EXPLAIN QUERY PLAN {sql}"), ())
                .await
                .unwrap();
            let mut plan = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                plan.push(row.get::<String>(3).unwrap());
            }

            // Both OR branches are index searches, so no step reads every
            // pull request.
            assert!(
                plan.iter()
                    .any(|step| step.contains("USING INDEX idx_pr_dependency")),
                "{plan:#?}"
            );
            assert!(
                !plan.iter().any(|step| step.starts_with("SCAN p")),
                "{plan:#?}"
            );
        }
    }

    #[tokio::test]
    async fn label_facets_are_ranked_by_count_then_name() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        // Popularity puts the alphabetically last label first and the
        // alphabetically first label last, so a key-sorted map cannot pass.
        // `go` and `Security` tie, and byte order would put `S` before `g`,
        // so the tie also checks that names compare case-insensitively.
        let labelled = [
            (1, vec!["rust", "dependencies"]),
            (2, vec!["rust", "Security", "go"]),
            (3, vec!["rust", "go"]),
            (4, vec!["rust", "Security"]),
        ];
        for (number, labels) in labelled {
            let mut record = pr(1, number, 10);
            record.labels = labels.into_iter().map(str::to_owned).collect();
            store.upsert_pr(&record).await.unwrap();
        }

        let summary = store.dashboard_summary(&PrFilter::default()).await.unwrap();

        assert_eq!(
            ranked_labels(&summary),
            [("rust", 4), ("go", 2), ("Security", 2), ("dependencies", 1)]
        );
    }

    fn ranked_labels(summary: &DashboardSummary) -> Vec<(&str, u64)> {
        summary
            .facets
            .labels
            .iter()
            .map(|facet| (facet.label.as_str(), facet.count))
            .collect()
    }

    fn repository_counts(summary: &DashboardSummary) -> Vec<(u64, u64)> {
        summary
            .facets
            .repositories
            .iter()
            .map(|facet| (facet.repository.repository_id, facet.count))
            .collect()
    }

    #[tokio::test]
    async fn check_and_update_type_facets_are_keyed_by_their_enums() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        for number in 1..=3 {
            store.upsert_pr(&pr(1, number, 10)).await.unwrap();
        }
        let mut failing_major = pr(1, 4, 10);
        failing_major.check_status = CheckStatus::Failure;
        failing_major.update_type = UpdateType::Major;
        store.upsert_pr(&failing_major).await.unwrap();

        let summary = store.dashboard_summary(&PrFilter::default()).await.unwrap();

        assert_eq!(
            summary.facets.checks,
            BTreeMap::from([(CheckStatus::Success, 3), (CheckStatus::Failure, 1)])
        );
        assert_eq!(
            summary.facets.update_types,
            BTreeMap::from([(UpdateType::Minor, 3), (UpdateType::Major, 1)])
        );
    }

    /// Four pull requests over two repositories, differing in check, update
    /// type, and labels, so every facet has something to hide and something
    /// to keep.
    async fn facet_fixture() -> (TempDir, LibSqlPrStore) {
        let (directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_repo(&repo(2, 10)).await.unwrap();
        let mut passing_minor_rust = pr(1, 1, 10);
        passing_minor_rust.labels = vec!["dependencies".to_owned(), "rust".to_owned()];
        let mut failing_major_go = pr(1, 2, 10);
        failing_major_go.check_status = CheckStatus::Failure;
        failing_major_go.update_type = UpdateType::Major;
        failing_major_go.labels = vec!["dependencies".to_owned(), "go".to_owned()];
        let mut failing_minor_rust = pr(2, 3, 10);
        failing_minor_rust.check_status = CheckStatus::Failure;
        failing_minor_rust.labels = vec!["dependencies".to_owned(), "rust".to_owned()];
        let mut passing_patch_security = pr(2, 4, 10);
        passing_patch_security.update_type = UpdateType::Patch;
        passing_patch_security.labels = vec!["security".to_owned()];
        for record in [
            passing_minor_rust,
            failing_major_go,
            failing_minor_rust,
            passing_patch_security,
        ] {
            store.upsert_pr(&record).await.unwrap();
        }
        (directory, store)
    }

    #[tokio::test]
    async fn each_facet_counts_within_the_other_filters_but_not_its_own() {
        let (_directory, store) = facet_fixture().await;
        let filter = PrFilter {
            check_statuses: vec![CheckStatus::Failure],
            labels: vec!["rust".to_owned()],
            ..Default::default()
        };

        let page = store.list_prs(&filter, Page::default()).await.unwrap();
        let summary = store.dashboard_summary(&filter).await.unwrap();

        // The rows honour every filter: only #3 is failing and rust.
        assert_eq!(page.total, 1);
        assert_eq!(
            page.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [3]
        );
        // Checks ignore the check filter and keep the label filter: the rust
        // pull requests are #1 (passing) and #3 (failing), so the sidebar
        // still offers the check that would widen the view.
        assert_eq!(
            summary.facets.checks,
            BTreeMap::from([(CheckStatus::Success, 1), (CheckStatus::Failure, 1)])
        );
        // Update types keep both filters: only #3 is left, and it is minor.
        assert_eq!(
            summary.facets.update_types,
            BTreeMap::from([(UpdateType::Minor, 1)])
        );
        // Labels ignore the label filter and keep the check filter: the
        // failing pull requests are #2 and #3.
        assert_eq!(
            ranked_labels(&summary),
            [("dependencies", 2), ("go", 1), ("rust", 1)]
        );
        // Repositories keep both filters: #3 lives in repo-2, and repo-1 is
        // still listed, at zero.
        assert_eq!(repository_counts(&summary), [(1, 0), (2, 1)]);
    }

    #[tokio::test]
    async fn the_repository_facet_ignores_the_repository_filter_and_the_rest_keep_it() {
        let (_directory, store) = facet_fixture().await;
        let filter = PrFilter {
            repos: vec!["acme/repo-1".to_owned()],
            check_statuses: vec![CheckStatus::Failure],
            ..Default::default()
        };

        let summary = store.dashboard_summary(&filter).await.unwrap();

        // Without the repository filter, the failing pull requests are #2 in
        // repo-1 and #3 in repo-2: choosing repo-2 instead would show one.
        assert_eq!(repository_counts(&summary), [(1, 1), (2, 1)]);
        // Every other facet is scoped to repo-1, whose only failing pull
        // request is #2, a major update labelled dependencies and go.
        assert_eq!(
            summary.facets.update_types,
            BTreeMap::from([(UpdateType::Major, 1)])
        );
        assert_eq!(ranked_labels(&summary), [("dependencies", 1), ("go", 1)]);
        // Checks are scoped to repo-1 with the check filter dropped: #1
        // passes and #2 fails.
        assert_eq!(
            summary.facets.checks,
            BTreeMap::from([(CheckStatus::Success, 1), (CheckStatus::Failure, 1)])
        );
    }

    /// The search box and the "needs attention" view are not facets, so they
    /// narrow every facet.
    #[tokio::test]
    async fn the_query_scopes_every_facet() {
        let (_directory, store) = facet_fixture().await;
        let mut tokio_update = pr(2, 5, 10);
        tokio_update.title = "Bump tokio from 1.0.0 to 1.1.0".to_owned();
        tokio_update.dependency = Some("tokio".to_owned());
        tokio_update.check_status = CheckStatus::Pending;
        tokio_update.labels = vec!["async".to_owned()];
        store.upsert_pr(&tokio_update).await.unwrap();
        let filter = PrFilter {
            query: Some("tokio".to_owned()),
            ..Default::default()
        };

        let summary = store.dashboard_summary(&filter).await.unwrap();

        assert_eq!(
            summary.facets.checks,
            BTreeMap::from([(CheckStatus::Pending, 1)])
        );
        assert_eq!(
            summary.facets.update_types,
            BTreeMap::from([(UpdateType::Minor, 1)])
        );
        assert_eq!(ranked_labels(&summary), [("async", 1)]);
        assert_eq!(repository_counts(&summary), [(1, 0), (2, 1)]);
    }

    #[tokio::test]
    async fn repository_facets_list_every_repository_grouped_by_owner_with_scoped_counts() {
        let (_directory, store) = test_store().await;
        // Owners arrive out of order and in mixed case; the facet must come
        // back with each owner's repositories together, owners and names
        // compared case-insensitively, and a repository without a matching
        // pull request listed at zero rather than dropped.
        let repositories = [
            (3, "beta", "api"),
            (2, "acme", "web"),
            (1, "acme", "API"),
            (4, "Zed", "tools"),
        ];
        for (id, owner, name) in repositories {
            store
                .upsert_repo(&RepoRecord {
                    owner: owner.to_owned(),
                    repo: name.to_owned(),
                    ..repo(id, 10)
                })
                .await
                .unwrap();
        }
        // acme/API has two major updates and a minor one; beta/api one major;
        // the other two repositories have no pull requests at all.
        let pull_requests = [
            (1, 1, "acme", "API", UpdateType::Major),
            (1, 2, "acme", "API", UpdateType::Major),
            (1, 3, "acme", "API", UpdateType::Minor),
            (3, 1, "beta", "api", UpdateType::Major),
        ];
        for (id, number, owner, name, update_type) in pull_requests {
            store
                .upsert_pr(&PrRecord {
                    owner: owner.to_owned(),
                    repo: name.to_owned(),
                    update_type,
                    ..pr(id, number, 10)
                })
                .await
                .unwrap();
        }
        let filter = PrFilter {
            update_types: vec![UpdateType::Major],
            ..Default::default()
        };

        let summary = store.dashboard_summary(&filter).await.unwrap();

        assert_eq!(
            summary
                .facets
                .repositories
                .iter()
                .map(|facet| {
                    (
                        facet.repository.owner.as_str(),
                        facet.repository.repo.as_str(),
                        facet.count,
                    )
                })
                .collect::<Vec<_>>(),
            [
                ("acme", "API", 2),
                ("acme", "web", 0),
                ("beta", "api", 1),
                ("Zed", "tools", 0),
            ]
        );
        assert_eq!(
            summary.facets.repositories[0].repository,
            RepoRecord {
                owner: "acme".to_owned(),
                repo: "API".to_owned(),
                ..repo(1, 10)
            },
            "the facet carries the whole repository row"
        );
    }

    #[tokio::test]
    async fn last_synced_at_is_the_newest_sync_or_absent_when_empty() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        let empty = store.dashboard_summary(&PrFilter::default()).await.unwrap();
        assert_eq!(empty.last_synced_at, None);

        store.upsert_pr(&pr(1, 1, 300)).await.unwrap();
        store.upsert_pr(&pr(1, 2, 700)).await.unwrap();
        store.upsert_pr(&pr(1, 3, 500)).await.unwrap();

        let synced = store
            .dashboard_summary(
                // The freshness stamp is for the whole projection, not the
                // rows the filter happens to leave visible.
                &PrFilter {
                    query: Some("no such pull request".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(synced.facets.checks, BTreeMap::new());
        assert_eq!(synced.last_synced_at, Some(700));
    }

    /// Watches the store's connection and records, for every `SELECT` SQLite
    /// compiles from now on, whether a transaction was open at the time.
    /// SQLite authorises subqueries as `SELECT`s of their own, so a statement
    /// with a subquery is recorded more than once. The hook holds a clone of
    /// the connection it is installed on; the cycle is fine for a test, whose
    /// connection is dropped with its temporary directory anyway.
    async fn watch_selects(store: &LibSqlPrStore) -> Arc<std::sync::Mutex<Vec<bool>>> {
        let connection = store.connection().await.clone();
        let in_transaction = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&in_transaction);
        let watched = connection.clone();
        connection
            .authorizer(Some(Arc::new(move |context: &libsql::AuthContext| {
                if context.action == libsql::AuthAction::Select {
                    recorded.lock().unwrap().push(!watched.is_autocommit());
                }
                libsql::Authorization::Allow
            })))
            .unwrap();
        in_transaction
    }

    /// A page is a count and some rows; a summary is several facets over one
    /// filter. Each is one answer, so a sync landing between its statements
    /// must not show in it: every statement reads inside the same
    /// transaction, and the transaction is over when the answer is.
    #[tokio::test]
    async fn a_page_and_a_summary_each_read_inside_one_transaction() {
        let (_directory, store) = test_store().await;
        let selects = watch_selects(&store).await;

        store
            .list_prs(&PrFilter::default(), Page::default())
            .await
            .unwrap();
        let page_selects = std::mem::take(&mut *selects.lock().unwrap());
        store.dashboard_summary(&PrFilter::default()).await.unwrap();
        let summary_selects = std::mem::take(&mut *selects.lock().unwrap());

        // A page is two statements, a summary five; subqueries add to the
        // count but never subtract from it.
        assert!(
            page_selects.len() >= 2 && page_selects.iter().all(|open| *open),
            "every SELECT of a page reads inside its transaction: {page_selects:?}"
        );
        assert!(
            summary_selects.len() >= 5 && summary_selects.iter().all(|open| *open),
            "every SELECT of a summary reads inside its transaction: {summary_selects:?}"
        );
        assert!(
            store.connection().await.is_autocommit(),
            "a finished read leaves no transaction behind"
        );
    }

    /// The dashboard polls the revision to learn whether anything it shows
    /// has changed, so every write that changes a row moves the projection's
    /// counter — the rows a repository delete takes with it included — and
    /// reads and writes that change nothing leave it where it is. The pull
    /// requests' counter follows their rows alone: a sweep writes every
    /// repository before it reaches a single pull request, and the dashboard
    /// must not take those writes for the pull requests having refreshed.
    #[tokio::test]
    async fn the_projection_revision_follows_every_row_and_the_pull_requests_one_only_theirs() {
        /// The revision as the dashboard follows it: what it last saw.
        struct Follower<'a> {
            store: &'a LibSqlPrStore,
            seen: ProjectionRevision,
        }

        impl Follower<'_> {
            /// A pull request row changed: both counters moved.
            async fn pull_requests_advanced(&mut self, what: &str) {
                let now = self.store.projection_revision().await.unwrap();
                assert!(
                    now.projection > self.seen.projection,
                    "{what} should advance the projection's revision"
                );
                assert!(
                    now.pull_requests > self.seen.pull_requests,
                    "{what} should advance the pull requests' revision"
                );
                self.seen = now;
            }

            /// Only repository rows changed: the projection's counter moved
            /// alone.
            async fn repositories_advanced(&mut self, what: &str) {
                let now = self.store.projection_revision().await.unwrap();
                assert!(
                    now.projection > self.seen.projection,
                    "{what} should advance the projection's revision"
                );
                assert_eq!(
                    now.pull_requests, self.seen.pull_requests,
                    "{what} should not advance the pull requests' revision"
                );
                self.seen = now;
            }

            /// No row changed: neither counter moved.
            async fn unchanged(&self, what: &str) {
                let now = self.store.projection_revision().await.unwrap();
                assert_eq!(now, self.seen, "{what} should not advance either revision");
            }
        }

        let (_directory, store) = test_store().await;
        let mut follower = Follower {
            seen: store.projection_revision().await.unwrap(),
            store: &store,
        };

        store.upsert_repo(&repo(1, 10)).await.unwrap();
        follower.repositories_advanced("adding a repository").await;
        store.upsert_repo(&repo(1, 20)).await.unwrap();
        follower
            .repositories_advanced("syncing a repository again")
            .await;
        store.upsert_pr(&pr(1, 1, 100)).await.unwrap();
        follower
            .pull_requests_advanced("adding a pull request")
            .await;
        store.upsert_pr(&pr(1, 1, 200)).await.unwrap();
        follower
            .pull_requests_advanced("syncing a pull request again")
            .await;

        store
            .list_prs(&PrFilter::default(), Page::default())
            .await
            .unwrap();
        store.dashboard_summary(&PrFilter::default()).await.unwrap();
        store.get_pr(&PrKey::new(1, 1)).await.unwrap();
        follower.unchanged("reading").await;
        store.retain_prs(1, &[1], 1_000).await.unwrap();
        follower
            .unchanged("a reconciliation that prunes nothing")
            .await;

        store.delete_pr(&PrKey::new(1, 1)).await.unwrap();
        follower
            .pull_requests_advanced("deleting a pull request")
            .await;
        store.upsert_pr(&pr(1, 2, 100)).await.unwrap();
        store.upsert_pr(&pr(1, 3, 100)).await.unwrap();
        follower
            .pull_requests_advanced("adding pull requests")
            .await;
        store.retain_prs(1, &[2], 1_000).await.unwrap();
        follower
            .pull_requests_advanced("a reconciliation that prunes a row")
            .await;

        store.upsert_repo(&repo(2, 10)).await.unwrap();
        follower
            .repositories_advanced("adding a second repository")
            .await;
        store.upsert_pr(&pr(2, 1, 100)).await.unwrap();
        follower
            .pull_requests_advanced("adding the second repository's pull request")
            .await;
        store
            .replace_installation_repos(9, &[repo(1, 30), repo(2, 30)], 1_000)
            .await
            .unwrap();
        follower
            .repositories_advanced("an installation sync that keeps every repository")
            .await;
        store
            .replace_installation_repos(9, &[repo(1, 40)], 1_000)
            .await
            .unwrap();
        follower
            .pull_requests_advanced(
                "an installation sync that drops a repository and its pull request",
            )
            .await;
        assert_eq!(store.get_pr(&PrKey::new(2, 1)).await.unwrap(), None);

        store.purge_installation(9).await.unwrap();
        follower
            .pull_requests_advanced("purging the installation")
            .await;
    }

    /// Migration 0004 promises that a repository delete which cascades to its
    /// pull requests moves the revision as surely as an upsert. The store
    /// never issues such a delete — it removes the pull requests first so it
    /// can report them — so exercise the cascade itself: one tick for the
    /// repository row and one for each pull request that went with it, of
    /// which only the pull requests' count towards their own revision.
    #[tokio::test]
    async fn a_cascading_repository_delete_advances_the_projection_revision() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 2, 10)).await.unwrap();
        let before = store.projection_revision().await.unwrap();

        store
            .connection()
            .await
            .execute(
                "DELETE FROM repositories WHERE repository_id = ?1",
                vec![integer(1).unwrap()],
            )
            .await
            .unwrap();

        assert_eq!(store.get_repo(1).await.unwrap(), None);
        assert_eq!(store.get_pr(&PrKey::new(1, 1)).await.unwrap(), None);
        assert_eq!(store.get_pr(&PrKey::new(1, 2)).await.unwrap(), None);
        assert_eq!(
            store.projection_revision().await.unwrap(),
            ProjectionRevision {
                projection: before.projection + 3,
                pull_requests: before.pull_requests + 2,
            }
        );
    }

    #[tokio::test]
    async fn needs_attention_surfaces_conflicting_pull_requests() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        // Both fixtures pass checks, are minor updates and were synced just
        // now, so mergeability is the only attention trigger in play.
        let now = unix_seconds();
        let clean = pr(1, 1, now);
        let mut conflicting = pr(1, 2, now);
        conflicting.mergeable = Mergeable::Dirty;
        store.upsert_pr(&clean).await.unwrap();
        store.upsert_pr(&conflicting).await.unwrap();

        let result = store
            .list_prs(
                &PrFilter {
                    needs_attention: true,
                    ..Default::default()
                },
                Page::default(),
            )
            .await
            .unwrap();

        assert_eq!(
            result.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [2]
        );
    }

    #[tokio::test]
    async fn legacy_mergeable_text_is_read_without_error() {
        let (directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        // Rows written before `Mergeable` existed hold GitHub's raw
        // `mergeable_state`, NULL, or (defensively) text this crate has never
        // heard of. None of them may make a pull request unreadable.
        let legacy: [(u64, Value, Mergeable); 4] = [
            (1, Value::Text("dirty".to_owned()), Mergeable::Dirty),
            (2, Value::Text("has_hooks".to_owned()), Mergeable::HasHooks),
            (3, Value::Null, Mergeable::Unknown),
            (4, Value::Text("conflicting".to_owned()), Mergeable::Unknown),
        ];
        let connection = sidecar(&directory).await;
        for (number, raw, _) in &legacy {
            let fixture = pr(1, *number, 10);
            connection
                .execute(
                    r#"INSERT INTO pull_requests (
                        id, repository_id, owner, repo, number, title, html_url,
                        dependencies, update_type, head_sha, check_status, mergeable,
                        labels, created_at, updated_at, synced_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '[]', ?8, ?9, ?10, ?11, '[]', 0, 0, 0)"#,
                    vec![
                        Value::Text(fixture.id.clone()),
                        integer(fixture.repository_id).unwrap(),
                        Value::Text(fixture.owner.clone()),
                        Value::Text(fixture.repo.clone()),
                        integer(fixture.number).unwrap(),
                        Value::Text(fixture.title.clone()),
                        Value::Text(fixture.html_url.clone()),
                        Value::Text(fixture.update_type.to_string()),
                        Value::Text(fixture.head_sha.clone()),
                        Value::Text(fixture.check_status.to_string()),
                        raw.clone(),
                    ],
                )
                .await
                .unwrap();
        }

        for (number, raw, expected) in legacy {
            let record = store
                .get_pr(&PrKey::new(1, number))
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("row {number} ({raw:?}) should be readable"));
            assert_eq!(record.mergeable, expected, "{raw:?}");
        }
    }

    #[tokio::test]
    async fn a_second_row_for_the_same_repository_and_number_is_rejected() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 7, 10)).await.unwrap();
        // The id is derived from (repository_id, number), so a row that
        // disagrees with its own id would be a second row for the same PR.
        let mut rogue = pr(1, 7, 10);
        rogue.id = "rogue".to_owned();

        let error = store.upsert_pr(&rogue).await.unwrap_err();

        assert!(
            error.to_string().contains(
                "UNIQUE constraint failed: pull_requests.repository_id, pull_requests.number"
            ),
            "{error}"
        );
        assert_eq!(
            error.class(),
            StoreErrorClass::Terminal,
            "a duplicate is a bug, and a fresh attempt would write the same row"
        );
        assert_eq!(
            store.get_pr(&PrKey::new(1, 7)).await.unwrap(),
            Some(pr(1, 7, 10))
        );
    }

    /// The classification tells the foreign key race apart from other
    /// constraint violations by SQLite's extended result code, which only
    /// works if libSQL hands that over rather than the bare primary code (19,
    /// SQLITE_CONSTRAINT, which is terminal). A real violation pins it: a
    /// pull request whose repository has not landed yet is a race with the
    /// repository sync, and worth another try.
    #[tokio::test]
    async fn a_pull_request_arriving_before_its_repository_is_worth_retrying() {
        let (_directory, store) = test_store().await;

        let error = store.upsert_pr(&pr(1, 1, 10)).await.unwrap_err();

        assert!(
            error.to_string().contains("FOREIGN KEY constraint failed"),
            "{error}"
        );
        assert_eq!(error.class(), StoreErrorClass::Retryable);
    }

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
    async fn a_repository_keeps_the_merge_method_it_was_synced_with() {
        let (_directory, store) = test_store().await;
        // `acme/repo-1` disallows the preferred method, so its sync resolved
        // a different one; `acme/repo-2` allows it and carries no override.
        let overridden = RepoRecord {
            merge_method: Some(MergeMethod::Rebase),
            ..repo(1, 10)
        };
        store
            .replace_installation_repos(9, &[overridden.clone(), repo(2, 10)], 20)
            .await
            .unwrap();

        assert_eq!(store.get_repo(1).await.unwrap(), Some(overridden));
        assert_eq!(store.get_repo(2).await.unwrap(), Some(repo(2, 10)));
        assert_eq!(store.get_repo(3).await.unwrap(), None);
        let listed = store
            .dashboard_summary(&PrFilter::default())
            .await
            .unwrap()
            .facets
            .repositories;
        assert_eq!(
            listed
                .iter()
                .map(|facet| (
                    facet.repository.repository_id,
                    facet.repository.merge_method
                ))
                .collect::<Vec<_>>(),
            [(1, Some(MergeMethod::Rebase)), (2, None)]
        );
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
    async fn retaining_repos_spares_rows_synced_since_the_listing_started() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_repo(&repo(2, 30)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(2, 1, 10)).await.unwrap();

        let cascaded = store.retain_repos(9, &[], 20).await.unwrap();

        assert_eq!(cascaded, vec![PrKey::new(1, 1)]);
        assert!(store.get_repo(1).await.unwrap().is_none());
        assert_eq!(
            store.get_repo(2).await.unwrap(),
            Some(repo(2, 30)),
            "a repository synced since the listing started was added behind the sweep's back"
        );
        assert!(store.get_pr(&PrKey::new(2, 1)).await.unwrap().is_some());
    }

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

    /// Watches the store's connection, as [`watch_selects`] does, and says
    /// when SQLite has compiled an `INSERT` into `table` on it. The hook runs
    /// as the statement is compiled, so by the time the notice arrives the
    /// insert is at the door of the write lock: its next step, on the same
    /// thread and with no await between, is to try the lock. A test that
    /// holds the lock from another connection can take the notice as the
    /// moment to let go of it, instead of guessing at a sleep. The notice is
    /// a permit, so it keeps if it is raised before anyone waits on it.
    ///
    /// The step is a few instructions after the notice; the other side's
    /// commit is a cross-thread wake and a write to disk. Should the commit
    /// still land in that gap, the insert passes without having met the lock
    /// — the gap is what is left of the race, libsql exposing no hook on the
    /// busy handler that would close it.
    async fn watch_insert_into(
        store: &LibSqlPrStore,
        table: &'static str,
    ) -> Arc<tokio::sync::Notify> {
        let connection = store.connection().await.clone();
        let compiled = Arc::new(tokio::sync::Notify::new());
        let notice = Arc::clone(&compiled);
        connection
            .authorizer(Some(Arc::new(move |context: &libsql::AuthContext| {
                if context.action == (libsql::AuthAction::Insert { table_name: table }) {
                    notice.notify_one();
                }
                libsql::Authorization::Allow
            })))
            .unwrap();
        compiled
    }

    /// The web and Restate processes share the file, so a write lands while
    /// the other is mid-transaction. The busy timeout is what makes it wait
    /// its turn instead of failing with SQLITE_BUSY; without it, the write
    /// here comes back with the error the moment it meets the lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writes_wait_for_a_writer_in_another_process() {
        let (directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();

        let other_process = sidecar(&directory).await;
        let transaction = other_process.transaction().await.unwrap();
        upsert_repo_on(&transaction, &repo(2, 10)).await.unwrap();

        // The insert runs on its own worker thread and blocks it inside
        // SQLite for as long as the other writer holds the lock; the notice
        // says when it is there.
        let insert_compiled = watch_insert_into(&store, "pull_requests").await;
        let write = tokio::spawn(async move { store.upsert_pr(&pr(1, 1, 10)).await });
        insert_compiled.notified().await;
        transaction.commit().await.unwrap();

        write.await.unwrap().unwrap();
    }

    /// A start-up that finds the schema current only reads the version. It
    /// must not queue behind a writer in the other process, let alone fail
    /// with SQLITE_BUSY: that is what would happen if it took the write lock
    /// to look.
    #[tokio::test]
    async fn connecting_to_a_current_database_does_not_wait_for_another_writer() {
        let (directory, _store) = test_store().await;
        let other_process = sidecar(&directory).await;
        let transaction = other_process
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .unwrap();

        let connected =
            LibSqlPrStore::connect(&StoreConfig::local(database_path(&directory))).await;

        // The other writer's lock was held from before `connect` until after
        // it returned: a `connect` that waited on the lock could only have
        // run out the busy timeout and failed with SQLITE_BUSY, and one that
        // met the lock without a timeout would have failed with it at once.
        // Coming back `Ok` is having done neither.
        transaction.commit().await.unwrap();
        connected.expect("connect neither waits on the other writer nor meets SQLITE_BUSY");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_transactions_on_cloned_stores_do_not_collide() {
        // Clones share one connection, so without serialisation one sync's
        // BEGIN would land inside the other's open transaction.
        let (_directory, store) = test_store().await;
        let syncs = [9_u64, 10].map(|installation| {
            let store = store.clone();
            tokio::spawn(async move {
                for round in 0..25 {
                    let mut record = repo(installation * 100 + round, 10);
                    record.installation_id = installation;
                    store
                        .replace_installation_repos(installation, &[record], 20)
                        .await?;
                }
                Ok::<_, StoreError>(())
            })
        });
        for sync in syncs {
            sync.await.unwrap().unwrap();
        }

        let summary = store.dashboard_summary(&PrFilter::default()).await.unwrap();
        let mut survivors = summary
            .facets
            .repositories
            .iter()
            .map(|facet| {
                (
                    facet.repository.installation_id,
                    facet.repository.repository_id,
                )
            })
            .collect::<Vec<_>>();
        survivors.sort_unstable();
        // Every round retires the previous round's repo, so exactly the last
        // one per installation remains.
        assert_eq!(survivors, [(9, 924), (10, 1024)]);
    }

    #[test]
    fn classifies_structured_sqlite_contention_and_the_foreign_key_race_as_retryable() {
        for code in [5, 6, 10, 5 | (2 << 8), 787] {
            let error = StoreError::Database(libsql::Error::SqliteFailure(code, "busy".into()));
            assert_eq!(error.class(), StoreErrorClass::Retryable, "code {code}");
        }
        // An embedded replica reports the primary and extended codes separately.
        let remote = StoreError::Database(libsql::Error::RemoteSqliteFailure(
            19,
            19 | (3 << 8),
            "constraint".into(),
        ));
        assert_eq!(remote.class(), StoreErrorClass::Retryable);
    }

    /// The messages are what libsql's remote connection puts in `Hrana` for a
    /// transport error, a dropped stream, a server error, a rate limit and a
    /// server-side SQLITE_BUSY; the class rests on the variant alone, for the
    /// reason `retryable_database_error` gives.
    #[test]
    fn classifies_a_remote_store_failure_as_retryable() {
        for message in [
            "http error: `connection reset by peer`",
            "stream closed: `stream expired`",
            "api error: `status=503, body=`",
            "api error: `status=429, body=too many requests`",
            "stream error: `Error { message: \"database is locked\", code: \"SQLITE_BUSY\" }`",
        ] {
            let error = StoreError::Database(libsql::Error::Hrana(message.into()));
            assert_eq!(error.class(), StoreErrorClass::Retryable, "{message}");
        }
    }

    #[test]
    fn classifies_unknown_and_data_errors_as_terminal() {
        let database = StoreError::Database(libsql::Error::Misuse("bad call".into()));
        assert_eq!(database.class(), StoreErrorClass::Terminal);
        let generic_constraint =
            StoreError::Database(libsql::Error::SqliteFailure(19, "constraint".into()));
        assert_eq!(generic_constraint.class(), StoreErrorClass::Terminal);
        // Primary key and unique violations are programming errors: a fresh
        // attempt would hit the same row.
        for code in [1555, 2067] {
            let error =
                StoreError::Database(libsql::Error::SqliteFailure(code, "constraint".into()));
            assert_eq!(error.class(), StoreErrorClass::Terminal, "code {code}");
        }
        let remote_unique = StoreError::Database(libsql::Error::RemoteSqliteFailure(
            19,
            19 | (8 << 8),
            "constraint".into(),
        ));
        assert_eq!(remote_unique.class(), StoreErrorClass::Terminal);
        assert_eq!(
            StoreError::IntegerOverflow.class(),
            StoreErrorClass::Terminal
        );
        assert_eq!(StoreError::MissingScalar.class(), StoreErrorClass::Terminal);
    }
}
