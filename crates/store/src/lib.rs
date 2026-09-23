use std::{
    collections::BTreeMap, env, path::Path, str::FromStr, sync::Arc, sync::LazyLock, time::Duration,
};

use async_trait::async_trait;
use dependaboard_core::{
    BatchRecord, BatchTargetRecord, DashboardPage, DashboardSummary, Mergeable, Page, PageCursor,
    PrFilter, PrKey, PrRecord, ProjectedBatch, ProjectionRevision, RepoRecord, Retirement,
    RunningBatch, unix_seconds,
};
use libsql::{Builder, Row, Value};
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::{Mutex, MutexGuard};

mod batches;
mod error;
mod facets;
mod filter;
mod migrations;
mod reconcile;
mod scope;
#[cfg(test)]
mod test_support;

pub use error::{StoreError, StoreErrorClass};

use batches::{
    batch_from_row, batch_target_from_row, running_batch_from_row, select_batch_targets_sql,
    select_batches_sql, select_running_batches_sql,
};
use facets::facet_counts;
use reconcile::{delete_repositories_where, retain_prs_on, retain_repos_on};
use scope::ScopedFilter;

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

    /// The pull request `key` names, or `None` when the projection has no row
    /// for it, whosever it is. The writers' lookup, and the one the store's
    /// own tests resolve to when they hold the concrete type with both traits
    /// in scope rather than being asked which trait they mean. The reader's
    /// side of the contract does not come through here: it must tell a
    /// foreign row from an absent one, and it reads a whole set of keys at
    /// once, so it has its own statement in
    /// [`ProjectionReader::get_prs`].
    async fn get_pr(&self, key: &PrKey) -> Result<Option<PrRecord>, StoreError> {
        let connection = self.connection().await;
        let mut rows = connection
            .query(
                &format!("{} WHERE p.id = ?1", select_pr_sql()),
                vec![Value::Text(key.to_string())],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(Some(pr_from_row(&row)?))
    }

    /// Holds the connection for the duration of one store operation. The
    /// lock is not re-entrant, so never call another store method (or
    /// `migrate`) while a guard is alive.
    async fn connection(&self) -> MutexGuard<'_, libsql::Connection> {
        self.connection.lock().await
    }
}

/// The half of the store's contract the Restate service holds: what its
/// handlers write after every state change, the lookups they make on the way —
/// a repository's row, the pull requests at a head, the retirement outbox — and
/// `get_pr`, the one method it shares with [`ProjectionReader`], so a writer
/// can read a row back without holding the reader. The dashboard's reads are
/// not here: a process that only writes need not implement them, and the
/// service's in-memory store does not.
#[async_trait]
pub trait ProjectionWriter: Send + Sync {
    /// Writes the row under the id `(repository_id, number)` derives, not the
    /// one the record carries, so no caller can store a pull request whose id
    /// disagrees with the pair every lookup names it by.
    async fn upsert_pr(&self, pr: &PrRecord) -> Result<(), StoreError>;
    async fn get_pr(&self, key: &PrKey) -> Result<Option<PrRecord>, StoreError>;
    async fn delete_pr(&self, key: &PrKey) -> Result<(), StoreError>;
    /// Reconciliation: drops this repository's rows that are not in `live` and
    /// were synced before the listing started, and reports which ones went.
    /// The `synced_before` guard keeps rows written by concurrent webhook
    /// syncs alive. The same keys are queued for retirement (see
    /// [`ProjectionWriter::pending_retirements`]) in the delete's own
    /// transaction, so the caller need not trust this call's return value to
    /// reach it.
    async fn retain_prs(
        &self,
        repository_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<Vec<PrKey>, StoreError>;
    async fn prs_for_sha(&self, repository_id: u64, sha: &str)
    -> Result<Vec<PrRecord>, StoreError>;
    async fn upsert_repo(&self, repo: &RepoRecord) -> Result<(), StoreError>;
    async fn get_repo(&self, repository_id: u64) -> Result<Option<RepoRecord>, StoreError>;
    /// Installation reconciliation, in one transaction: upserts every repository in
    /// `repos`, then drops the installation's other repositories that were synced
    /// before the listing started, pull requests and all. Reports the pull requests
    /// that went with them; the same keys are queued for retirement under
    /// `synced_before` as their fence. The guard keeps repositories added by
    /// concurrent syncs alive.
    async fn replace_installation_repos(
        &self,
        installation_id: u64,
        repos: &[RepoRecord],
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
    /// [`ProjectionWriter::pending_retirements`] read returned last. Ids only
    /// grow, so anything queued since that read stays for the next drain;
    /// acknowledging again is a no-op.
    async fn acknowledge_retirements(&self, through: u64) -> Result<(), StoreError>;
    /// Lists a bulk action as running until [`ProjectionWriter::record_batch`]
    /// keeps it as finished, or [`ProjectionWriter::unlist_batch`] gives it
    /// up. A batch already listed is left as it was, for the same reason a
    /// recorded one is.
    async fn start_batch(&self, batch: &RunningBatch) -> Result<(), StoreError>;
    /// Stops listing a bulk action as running without recording it: the
    /// workflow ended without a finished batch to keep, as a cancelled one
    /// does. A batch not listed is left as it is.
    async fn unlist_batch(&self, batch_id: &str) -> Result<(), StoreError>;
    /// Keeps a finished bulk action for the audit view, targets and all, and
    /// stops listing it as running. A batch already recorded is left as it
    /// was: Restate may run the recording step again when the first attempt's
    /// result was lost, and the first word is the one that stands.
    async fn record_batch(&self, batch: &BatchRecord) -> Result<(), StoreError>;
}

/// What the projection holds for a key, read within one installation.
///
/// Three answers rather than two, because the two ways of holding nothing
/// are told apart differently by the caller. A key with no row anywhere is an
/// answer — the pull request has been merged or closed since the dashboard
/// drew it — while a key whose row belongs to another installation is not:
/// this deployment's projection never showed that row, so no dashboard of
/// its could have named it, and the web edge refuses the request rather than
/// telling the browser its pull request has gone. Folding the two into
/// `None` would quietly turn that refusal into "no longer in the dashboard".
///
/// One read decides it, so the row and the verdict on it come from the same
/// snapshot. The row is boxed so the answer is a pointer wide whichever way
/// it falls, rather than a whole [`PrRecord`] wide to say there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectedPr {
    /// The row, in the installation it was asked for.
    Row(Box<PrRecord>),
    /// No row under any installation.
    Absent,
    /// A row, but one of another installation: not this deployment's to read.
    Foreign,
}

/// The half of the store's contract the web app's server holds: what the
/// dashboard reads — the rows and the facets around them, the revision it
/// polls, the batches it lists for audit — and `get_pr`, through which every
/// server function that takes a key from the browser resolves the key. Nothing
/// here writes, so a process holding this half alone cannot reach the write
/// half however it tries.
///
/// Every pull-request read takes the installation it is for and holds its
/// answer to it, so a store shared by two deployments shows each only its
/// own: the rows and their total, every facet, the freshness stamp, the
/// batches, and a key resolved on its own. The one exception is
/// [`ProjectionReader::projection_revision`], and its doc says why.
#[async_trait]
pub trait ProjectionReader: Send + Sync {
    /// What the projection holds for each of `keys` within `installation_id`,
    /// each told apart three ways so the caller need not work out whose a row
    /// is to know which answer it got — a [`PrRecord`] says nothing of
    /// installations, and the repository row it hangs off is what does.
    /// Unlike [`ProjectionWriter::get_pr`], which serves a service that owns
    /// every row it names, this one is asked whose each row is, because the
    /// keys came from a browser.
    ///
    /// **The answers align positionally with `keys`**: the nth answer is the
    /// nth key's, whatever the projection holds for it and whatever order the
    /// database produced its rows in. That is part of the interface, not an
    /// accident of an implementation — a caller pairs the two up by position,
    /// and reports what it leaves out in the order it was asked. A key
    /// repeated in `keys` is answered at each of its positions, and an empty
    /// `keys` is an empty answer, not a read.
    ///
    /// The plural is the lookup: a batch resolves a hundred keys at once, and
    /// a store serialises its reads, so a key at a time is a hundred round
    /// trips with the browser waiting on all of them.
    /// [`ProjectionReader::get_pr`] is this one for a single key.
    async fn get_prs(
        &self,
        installation_id: u64,
        keys: &[PrKey],
    ) -> Result<Vec<ProjectedPr>, StoreError>;
    /// [`ProjectionReader::get_prs`] for the one key the caller has, which is
    /// what a read of a row, a per-pull-request sync, or the drawer asks for.
    /// Provided, not implemented: there is one lookup on this trait, and this
    /// is the shape most of its callers want it in.
    async fn get_pr(&self, installation_id: u64, key: &PrKey) -> Result<ProjectedPr, StoreError> {
        Ok(self
            .get_prs(installation_id, std::slice::from_ref(key))
            .await?
            .into_iter()
            .next()
            // Not a failure the store can have: one key is one answer, by the
            // alignment `get_prs` promises, so nothing here is an implementer
            // having broken its own contract.
            .expect("one key is answered with one answer"))
    }
    /// One keyset page of `installation_id`'s pull requests that `filter`
    /// matches, newest update first, plus how many match in all. Another
    /// installation's rows are neither listed nor counted.
    async fn list_prs(
        &self,
        installation_id: u64,
        filter: &PrFilter,
        page: Page,
    ) -> Result<DashboardPage, StoreError>;
    /// What frames the rows for `filter` within `installation_id`: every
    /// facet's counts, each scoped to the filter minus its own dimension, and
    /// the read model's freshness. It does not depend on paging, so a caller
    /// moving to the next page need not ask again. The repository facet lists
    /// the installation's repositories, not the database's.
    async fn dashboard_summary(
        &self,
        installation_id: u64,
        filter: &PrFilter,
    ) -> Result<DashboardSummary, StoreError>;
    /// Two counters, one that moves whenever a row of the read model changes,
    /// however it changes, and one that moves only when a pull request row
    /// does. Cheap to read, so a dashboard can ask often and act only when
    /// an answer differs from the one it last saw.
    ///
    /// Deployment-wide on purpose, and the one read here that is: both
    /// counters are a single row kept by table-wide triggers, so a shared
    /// store moves them for the other installation's writes too. The cost is
    /// a dashboard that reloads and finds nothing changed; the alternative is
    /// a counter per installation, which every write would have to resolve
    /// through its repository's row. A poll is not worth that, so the
    /// asymmetry stands.
    async fn projection_revision(&self) -> Result<ProjectionRevision, StoreError>;
    /// The `limit` most recently finished batches of `installation_id`, newest
    /// first, each with every target's verdict in batch order. Another
    /// installation's batches are not counted against the limit, and a batch
    /// attributed to no installation is nobody's to read.
    async fn recent_batches(
        &self,
        installation_id: u64,
        limit: u32,
    ) -> Result<Vec<BatchRecord>, StoreError>;
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
impl ProjectionWriter for LibSqlPrStore {
    async fn upsert_pr(&self, pr: &PrRecord) -> Result<(), StoreError> {
        let connection = self.connection().await;
        let dependencies = serde_json::to_string(&pr.dependencies)?;
        let labels = serde_json::to_string(&pr.labels)?;
        // Derived, not taken from the record: see the trait's doc comment.
        let id = pr.key().to_string();
        // Named one by one, and exhaustively, so a column the record gains
        // stops compiling here rather than going quietly unwritten. The read
        // side has this already — `pr_from_row` builds the struct, so it
        // cannot miss a field and still compile — and this is its other half.
        // The two `_`s are the fields serialised into the locals above, whose
        // names they would otherwise shadow.
        let PrRecord {
            id: _,
            repository_id,
            owner,
            repo,
            number,
            title,
            html_url,
            dependency,
            from_version,
            to_version,
            dependencies: _,
            update_type,
            head_sha,
            check_status,
            mergeable,
            labels: _,
            created_at,
            updated_at,
            synced_at,
        } = pr;
        connection
            .execute(
                upsert_pr_sql(),
                vec![
                    Value::Text(id),
                    integer(*repository_id)?,
                    Value::Text(owner.clone()),
                    Value::Text(repo.clone()),
                    integer(*number)?,
                    Value::Text(title.clone()),
                    Value::Text(html_url.clone()),
                    option_text(dependency.clone()),
                    option_text(from_version.clone()),
                    option_text(to_version.clone()),
                    Value::Text(dependencies),
                    Value::Text(update_type.to_string()),
                    Value::Text(head_sha.clone()),
                    Value::Text(check_status.to_string()),
                    Value::Text(mergeable.to_string()),
                    Value::Text(labels),
                    integer(*created_at)?,
                    integer(*updated_at)?,
                    integer(*synced_at)?,
                ],
            )
            .await?;
        Ok(())
    }

    async fn get_pr(&self, key: &PrKey) -> Result<Option<PrRecord>, StoreError> {
        LibSqlPrStore::get_pr(self, key).await
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
}

#[async_trait]
impl ProjectionReader for LibSqlPrStore {
    /// One statement for the whole set, and it answers all three ways at
    /// once: the keys match on `p.id`, which is what a [`PrKey`] spells, and
    /// `select_pr_sql` joins `repositories`, so each row arrives with the
    /// installation that owns it. Nothing is held to `installation_id` in the
    /// `WHERE` — a foreign row must come back to be told from an absent one —
    /// and a key no row came back for is the absent one. A hundred keys is a
    /// hundred parameters, well inside SQLite's limit, and one round trip on
    /// a connection every other read is queued behind.
    ///
    /// The rows are gathered by id and then read off in `keys` order, which
    /// is what makes the answers positional: the database is free to produce
    /// an `IN` set in whatever order it likes, and a repeated key gets its
    /// answer at each position rather than at the first.
    async fn get_prs(
        &self,
        installation_id: u64,
        keys: &[PrKey],
    ) -> Result<Vec<ProjectedPr>, StoreError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = (1..=keys.len())
            .map(|position| format!("?{position}"))
            .collect::<Vec<_>>()
            .join(", ");
        let parameters = keys
            .iter()
            .map(|key| Value::Text(key.to_string()))
            .collect::<Vec<_>>();
        let connection = self.connection().await;
        let mut rows = connection
            .query(
                &format!("{} WHERE p.id IN ({placeholders})", select_pr_sql()),
                parameters,
            )
            .await?;
        let mut found = BTreeMap::new();
        while let Some(row) = rows.next().await? {
            let record = pr_from_row(&row)?;
            found.insert(record.id.clone(), (record, installation_from_row(&row)?));
        }
        Ok(keys
            .iter()
            .map(|key| match found.get(&key.to_string()) {
                None => ProjectedPr::Absent,
                Some((row, owner)) if *owner == installation_id => {
                    ProjectedPr::Row(Box::new(row.clone()))
                }
                Some(_) => ProjectedPr::Foreign,
            })
            .collect())
    }

    async fn list_prs(
        &self,
        installation_id: u64,
        filter: &PrFilter,
        page: Page,
    ) -> Result<DashboardPage, StoreError> {
        let connection = self.connection().await;
        // The count and the page are one answer, so they read one snapshot:
        // a sync landing between them must not leave a total that disagrees
        // with the rows.
        let transaction = connection.transaction().await?;
        let now = unix_seconds();
        let scoped = ScopedFilter::new(filter, None, now, installation_id)?;
        let count = count_sql(scoped.where_sql());
        let total = scalar_u64(&transaction, &count, scoped.into_params()).await?;

        let cursor = page.after.as_deref().map(PageCursor::decode).transpose()?;
        let mut scoped = ScopedFilter::new(filter, cursor.as_ref(), now, installation_id)?;
        let limit = page.normalized_limit() as usize;
        // One extra row is what tells the page there is another after it.
        let sql = page_sql(&mut scoped, (limit + 1) as u64)?;
        let page_rows = transaction.query(&sql, scoped.into_params()).await?;
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

    async fn dashboard_summary(
        &self,
        installation_id: u64,
        filter: &PrFilter,
    ) -> Result<DashboardSummary, StoreError> {
        let connection = self.connection().await;
        // Every facet frames the same rows, so they all count one snapshot:
        // a sync landing between them must not leave facets that do not add
        // up to each other.
        let transaction = connection.transaction().await?;
        let now = unix_seconds();
        let facets = facet_counts(&transaction, installation_id, filter, now).await?;
        // The stamp is for the whole of this deployment's projection, so it
        // ignores the filter — but not the installation: another
        // deployment's sweep is not this one's freshness.
        let scoped = ScopedFilter::new(&PrFilter::default(), None, now, installation_id)?;
        let freshness = format!(
            "SELECT MAX(p.synced_at) FROM pull_requests p {}",
            scoped.where_sql()
        );
        let last_synced_at =
            scalar_optional_u64(&transaction, &freshness, scoped.into_params()).await?;
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

/// The columns of `pull_requests`, in the order the projection writes them and
/// reads them back. Minted from one list rather than written out per
/// statement, so a column the record gains cannot reach the insert and miss
/// the rewrite, or reach both and miss the projection a reader decodes — the
/// argument [`repo_columns`] makes for the other table, with four statements
/// to keep in step here instead of two.
///
/// The order is the decoder's: [`pr_from_row`] reads `0..PR_COLUMNS.len()`
/// straight through, so an entry's place here is the index that reads it.
const PR_COLUMNS: [&str; 19] = [
    "id",
    "repository_id",
    "owner",
    "repo",
    "number",
    "title",
    "html_url",
    "dependency",
    "from_version",
    "to_version",
    "dependencies",
    "update_type",
    "head_sha",
    "check_status",
    "mergeable",
    "labels",
    "created_at",
    "updated_at",
    "synced_at",
];

/// Where [`select_pr_sql`] puts the installation it joins in: past the pull
/// request's own columns, so its index is the roster's length rather than a
/// literal in the middle that every column after it has to be counted around.
const PR_INSTALLATION_INDEX: i32 = PR_COLUMNS.len() as i32;

/// [`PR_COLUMNS`] under `alias`, for the statements that name the table.
fn pr_columns(alias: &str) -> String {
    PR_COLUMNS
        .map(|column| format!("{alias}{column}"))
        .join(", ")
}

/// The projection's write for one pull request: insert it, or rewrite every
/// column but the one it is keyed by. Rendered from [`PR_COLUMNS`] so the
/// three forms a column takes in this statement — its name, its placeholder,
/// and its `excluded` assignment — are one list, and the parameter vector the
/// caller binds is the only thing left to keep in step by hand.
fn upsert_pr_sql() -> &'static str {
    static SQL: LazyLock<String> = LazyLock::new(|| {
        let columns = PR_COLUMNS.join(", ");
        let placeholders = (1..=PR_COLUMNS.len())
            .map(|position| format!("?{position}"))
            .collect::<Vec<_>>()
            .join(", ");
        // `id` is the conflict target, so it is the one column a rewrite
        // leaves alone — and the one value derived rather than taken from the
        // record, which is why it could never be rewritten from `excluded`.
        let rewrite = PR_COLUMNS
            .iter()
            .filter(|column| **column != "id")
            .map(|column| format!("{column} = excluded.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO pull_requests ({columns}) VALUES ({placeholders}) \
             ON CONFLICT(id) DO UPDATE SET {rewrite}"
        )
    });
    &SQL
}

fn select_pr_sql() -> &'static str {
    static SQL: LazyLock<String> = LazyLock::new(|| {
        format!(
            "SELECT {}, r.installation_id \
             FROM pull_requests p \
             JOIN repositories r ON r.repository_id = p.repository_id",
            pr_columns("p.")
        )
    });
    &SQL
}

/// The dashboard's total for a [`ScopedFilter`]'s clause.
fn count_sql(where_sql: &str) -> String {
    format!("SELECT COUNT(*) FROM pull_requests p {where_sql}")
}

/// One keyset page for `scoped`; the limit binds through it, so the
/// placeholder this renders is the position the value took.
fn page_sql(scoped: &mut ScopedFilter, limit: u64) -> Result<String, StoreError> {
    let limit = scoped.bind(integer(limit)?);
    Ok(format!(
        "{} {} ORDER BY p.updated_at DESC, p.id DESC LIMIT {limit}",
        select_pr_sql(),
        scoped.where_sql()
    ))
}

/// The pull request's own columns of a [`select_pr_sql`] row, which are
/// [`PR_COLUMNS`] in order; the installation joined in past them is
/// [`installation_from_row`]'s.
fn pr_from_row(row: &Row) -> Result<PrRecord, StoreError> {
    Ok(PrRecord {
        id: row.get(0)?,
        repository_id: unsigned(row.get::<i64>(1)?)?,
        owner: row.get(2)?,
        repo: row.get(3)?,
        number: unsigned(row.get::<i64>(4)?)?,
        title: row.get(5)?,
        html_url: row.get(6)?,
        dependency: row.get(7)?,
        from_version: row.get(8)?,
        to_version: row.get(9)?,
        dependencies: serde_json::from_str(&row.get::<String>(10)?)?,
        update_type: stored_enum(row.get(11)?)?,
        head_sha: row.get(12)?,
        check_status: stored_enum(row.get(13)?)?,
        // The column is nullable and was once written verbatim from GitHub, so
        // NULL and any unrecognised legacy text deliberately fold into Unknown
        // rather than surfacing as CorruptEnum.
        mergeable: row
            .get::<Option<String>>(14)?
            .as_deref()
            .map_or(Mergeable::Unknown, Mergeable::from_github_state),
        labels: serde_json::from_str(&row.get::<String>(15)?)?,
        created_at: unsigned(row.get::<i64>(16)?)?,
        updated_at: unsigned(row.get::<i64>(17)?)?,
        synced_at: unsigned(row.get::<i64>(18)?)?,
    })
}

/// The installation of the repository a [`select_pr_sql`] row's pull request
/// hangs off, joined in past the pull request's own columns at
/// [`PR_INSTALLATION_INDEX`]. The pull request's own columns hold no
/// installation: the repository row is what says whose it is.
fn installation_from_row(row: &Row) -> Result<u64, StoreError> {
    unsigned(row.get::<i64>(PR_INSTALLATION_INDEX)?)
}

/// Drains the rows of a [`select_pr_sql`] query, in the order the database
/// produced them. Taking the rows by value means nothing of the statement
/// outlives the call, so a caller can end its transaction right after.
async fn collect_prs(mut rows: libsql::Rows) -> Result<Vec<PrRecord>, StoreError> {
    let mut records = Vec::new();
    while let Some(row) = rows.next().await? {
        records.push(pr_from_row(&row)?);
    }
    Ok(records)
}

/// Parses a column that persists an enum's `Display` form. Unrecognised text
/// is corrupt data.
fn stored_enum<T: FromStr>(value: String) -> Result<T, StoreError> {
    T::from_str(&value).map_err(|_| StoreError::CorruptEnum(value))
}

/// The `repositories` columns in the order [`repo_from_row`] reads them, each
/// qualified with `alias`: empty for a statement that names the table alone,
/// `"r."` for one that binds it beside another. Minted from one list rather
/// than written out per statement, so a column the mapper gains cannot reach
/// one reader's `SELECT` and miss the other's.
/// The columns of `repositories`, in the order [`repo_from_row`] reads them.
pub(crate) const REPO_COLUMNS: [&str; 6] = [
    "repository_id",
    "installation_id",
    "owner",
    "repo",
    "merge_method",
    "synced_at",
];

/// Where a statement that appends a column of its own to [`REPO_COLUMNS`]
/// finds it: past the repository's own, so its index is the roster's length.
/// The repository facet appends a count that way.
pub(crate) const REPO_EXTRA_INDEX: i32 = REPO_COLUMNS.len() as i32;

pub(crate) fn repo_columns(alias: &str) -> String {
    REPO_COLUMNS
        .map(|column| format!("{alias}{column}"))
        .join(", ")
}

fn select_repo_sql() -> String {
    format!("SELECT {} FROM repositories", repo_columns(""))
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

#[cfg(test)]
mod tests {
    use dependaboard_core::{CheckStatus, DependencyUpdate, MergeMethod, UpdateType};

    use crate::test_support::{
        INSTALLATION, OTHER_INSTALLATION, database_path, pr, repo, sidecar, test_store,
    };

    use super::*;

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

    /// A deployment serves one installation, and every pull request read is
    /// held to it: the rows, the total, the freshness stamp, and every facet
    /// — the repository facet included, which lists repositories directly and
    /// so would otherwise offer another installation's repositories with
    /// nothing in them. Two installations share this store, and nothing of
    /// the other one's is visible from this one's reads.
    #[tokio::test]
    async fn every_pull_request_read_is_held_to_the_installation() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store
            .upsert_repo(&RepoRecord {
                installation_id: OTHER_INSTALLATION,
                ..repo(2, 10)
            })
            .await
            .unwrap();
        for number in [1, 2] {
            store.upsert_pr(&pr(1, number, 10)).await.unwrap();
        }
        // The other installation's row differs in every dimension the
        // sidebar counts, and was synced later, so each facet and the
        // freshness stamp would change visibly if it were counted.
        let mut foreign = pr(2, 3, 900);
        foreign.check_status = CheckStatus::Failure;
        foreign.update_type = UpdateType::Major;
        foreign.labels = vec!["go".to_owned()];
        store.upsert_pr(&foreign).await.unwrap();

        let page = store
            .list_prs(INSTALLATION, &PrFilter::default(), Page::default())
            .await
            .unwrap();
        let summary = store
            .dashboard_summary(INSTALLATION, &PrFilter::default())
            .await
            .unwrap();

        assert_eq!(page.total, 2);
        assert_eq!(
            page.rows.iter().map(|row| row.number).collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(
            summary
                .facets
                .repositories
                .iter()
                .map(|facet| (facet.repository.repository_id, facet.count))
                .collect::<Vec<_>>(),
            [(1, 2)],
            "the other installation's repository is not listed at all"
        );
        assert_eq!(
            summary.facets.checks,
            BTreeMap::from([(CheckStatus::Success, 2)])
        );
        assert_eq!(
            summary.facets.update_types,
            BTreeMap::from([(UpdateType::Minor, 2)])
        );
        assert_eq!(
            summary
                .facets
                .labels
                .iter()
                .map(|facet| (facet.label.as_str(), facet.count))
                .collect::<Vec<_>>(),
            [("dependencies", 2), ("rust", 2)]
        );
        assert_eq!(
            summary.last_synced_at,
            Some(10),
            "the freshness stamp follows this installation's syncs alone"
        );
    }

    /// A pull request is of whichever installation its repository's row is
    /// of — the row itself says nothing about it — and resolving a key within
    /// an installation answers three ways, not two: a key with no row
    /// anywhere is nothing to show, while a key whose repository is another
    /// installation's is a key this deployment's projection never showed. The
    /// web edge tells the browser different things about them, so the store
    /// must not fold them together.
    #[tokio::test]
    async fn the_repository_a_pull_request_hangs_off_decides_whose_it_is_and_a_foreign_key_is_not_an_absent_one()
     {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store
            .upsert_repo(&RepoRecord {
                installation_id: OTHER_INSTALLATION,
                ..repo(2, 10)
            })
            .await
            .unwrap();
        // Both pull requests are written the same way; only their
        // repositories differ, and that is what parts them.
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(2, 3, 10)).await.unwrap();

        assert_eq!(
            ProjectionReader::get_pr(&store, INSTALLATION, &PrKey::new(1, 1))
                .await
                .unwrap(),
            ProjectedPr::Row(Box::new(pr(1, 1, 10)))
        );
        assert_eq!(
            ProjectionReader::get_pr(&store, INSTALLATION, &PrKey::new(2, 3))
                .await
                .unwrap(),
            ProjectedPr::Foreign
        );
        assert_eq!(
            ProjectionReader::get_pr(&store, INSTALLATION, &PrKey::new(1, 99))
                .await
                .unwrap(),
            ProjectedPr::Absent
        );
        // And the other deployment reads the same store the other way round.
        assert_eq!(
            ProjectionReader::get_pr(&store, OTHER_INSTALLATION, &PrKey::new(2, 3))
                .await
                .unwrap(),
            ProjectedPr::Row(Box::new(pr(2, 3, 10)))
        );
    }

    /// The set read answers by position, not by what came back. The keys go
    /// in an order no `IN` clause need honour, all three answers among them
    /// and one key asked twice, and each position gets its own key's answer:
    /// a caller pairs its own list up with this one by index, so an answer
    /// out of place would be an answer about the wrong pull request. No keys
    /// is no answers, and no read.
    #[tokio::test]
    async fn a_set_read_answers_every_key_at_its_own_position_whatever_order_the_rows_came_back_in()
    {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store
            .upsert_repo(&RepoRecord {
                installation_id: OTHER_INSTALLATION,
                ..repo(2, 10)
            })
            .await
            .unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 4, 10)).await.unwrap();
        store.upsert_pr(&pr(2, 3, 10)).await.unwrap();

        let answers = store
            .get_prs(
                INSTALLATION,
                &[
                    PrKey::new(2, 3),
                    PrKey::new(1, 99),
                    PrKey::new(1, 4),
                    PrKey::new(1, 99),
                    PrKey::new(1, 1),
                ],
            )
            .await
            .unwrap();

        assert_eq!(
            answers,
            vec![
                ProjectedPr::Foreign,
                ProjectedPr::Absent,
                ProjectedPr::Row(Box::new(pr(1, 4, 10))),
                ProjectedPr::Absent,
                ProjectedPr::Row(Box::new(pr(1, 1, 10))),
            ]
        );
        assert_eq!(store.get_prs(INSTALLATION, &[]).await.unwrap(), Vec::new());
    }

    /// The roster the statements are minted from and the table the migrations
    /// build name the same columns. A migration that adds one the roster never
    /// learns of is a column nothing writes and nothing reads; a roster entry
    /// the table has not got is a statement SQLite refuses, which every other
    /// test here would already be shouting about. This is the direction
    /// nothing else covers: the migrations are only ever compared with each
    /// other (`migrations::tests`), so a column added to the schema alone is
    /// silent everywhere.
    ///
    /// By name and not by position: a later `ALTER TABLE ... ADD COLUMN`
    /// appends physically wherever SQLite likes, while the roster's order is
    /// the decoder's, and the two have no reason to agree.
    #[tokio::test]
    async fn the_roster_names_exactly_the_columns_the_pull_requests_table_has() {
        let (_directory, store) = test_store().await;
        let connection = store.connection().await;

        let mut declared = Vec::new();
        let mut rows = connection
            .query("PRAGMA table_info(pull_requests)", ())
            .await
            .unwrap();
        while let Some(row) = rows.next().await.unwrap() {
            declared.push(row.get::<String>(1).unwrap());
        }
        declared.sort_unstable();

        let mut rostered = PR_COLUMNS.map(str::to_owned).to_vec();
        rostered.sort_unstable();

        assert_eq!(declared, rostered);
    }

    /// One pull request whose every field of a given type holds a value no
    /// other field of that type holds, so no two columns can trade places and
    /// come back looking right. `generation` picks a second, wholly different
    /// set of values for the same pull request, for the write that has to go
    /// through `ON CONFLICT ... DO UPDATE SET` rather than through the insert.
    ///
    /// The integers are the ones to watch: `repository_id`, `number` and the
    /// three stamps are stored side by side and read back by position, so
    /// equal values there are what would let a swap pass unseen.
    fn distinctly_valued_pr(generation: u64) -> PrRecord {
        let tag = |what: &str| format!("{what}-{generation}");
        PrRecord {
            id: PrKey::new(41, 53).to_string(),
            repository_id: 41,
            owner: tag("owner"),
            repo: tag("repo"),
            number: 53,
            title: tag("title"),
            html_url: tag("html-url"),
            dependency: Some(tag("dependency")),
            from_version: Some(tag("from-version")),
            to_version: Some(tag("to-version")),
            dependencies: vec![DependencyUpdate {
                name: tag("grouped-name"),
                from_version: Some(tag("grouped-from")),
                to_version: Some(tag("grouped-to")),
                update_type: UpdateType::Patch,
            }],
            update_type: if generation == 1 {
                UpdateType::Major
            } else {
                UpdateType::Patch
            },
            head_sha: tag("head-sha"),
            check_status: if generation == 1 {
                CheckStatus::Failure
            } else {
                CheckStatus::Pending
            },
            mergeable: if generation == 1 {
                Mergeable::Behind
            } else {
                Mergeable::Draft
            },
            labels: vec![tag("label")],
            created_at: 60 + generation,
            updated_at: 70 + generation,
            synced_at: 80 + generation,
        }
    }

    /// Every column the projection keeps for a pull request goes to its own
    /// place and comes back from it: through the insert, through the
    /// `excluded` rewrite a second write of the same key takes, and through
    /// each of the four reads that decode a row.
    ///
    /// The second write is the half nothing else covers. The column list, the
    /// placeholders, the `excluded` assignments, the parameter vector and the
    /// decoder are five hand-written statements of one order, and only the
    /// first write is exercised anywhere: a row written once and read back
    /// says nothing about the eighteen `x = excluded.x` lines that carry every
    /// write after it. Drop `synced_at` from them and staleness never
    /// refreshes; drop `head_sha` and a check webhook stops finding the pull
    /// request at the head it just reported.
    #[tokio::test]
    async fn every_column_of_a_pull_request_survives_a_write_a_rewrite_and_each_read_path() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(41, 10)).await.unwrap();

        for generation in 1..=2 {
            let written = distinctly_valued_pr(generation);
            let key = written.key();

            store.upsert_pr(&written).await.unwrap();

            assert_eq!(
                LibSqlPrStore::get_pr(&store, &key).await.unwrap().as_ref(),
                Some(&written),
                "the writer's own lookup, generation {generation}"
            );
            assert_eq!(
                ProjectionReader::get_pr(&store, INSTALLATION, &key)
                    .await
                    .unwrap(),
                ProjectedPr::Row(Box::new(written.clone())),
                "the reader's lookup, generation {generation}"
            );
            assert_eq!(
                store
                    .get_prs(INSTALLATION, std::slice::from_ref(&key))
                    .await
                    .unwrap(),
                vec![ProjectedPr::Row(Box::new(written.clone()))],
                "the set read, generation {generation}"
            );
            assert_eq!(
                store
                    .list_prs(INSTALLATION, &PrFilter::default(), Page::default())
                    .await
                    .unwrap()
                    .rows,
                vec![written.clone()],
                "the dashboard's page, generation {generation}"
            );
            assert_eq!(
                store
                    .prs_for_sha(written.repository_id, &written.head_sha)
                    .await
                    .unwrap(),
                vec![written.clone()],
                "the head lookup, generation {generation}"
            );
        }
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
                INSTALLATION,
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
                INSTALLATION,
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

    /// A page is offered a page after it when a row was found past its limit,
    /// and not when the rows ran out exactly on it. The fence is the one row
    /// read past the limit, and the test above cannot stand on it: three rows
    /// taken two at a time leave a last page one row short, so the read finds
    /// nothing past the limit whether the count is compared with `>` or with
    /// `>=`. Four rows taken two at a time land the last page exactly on the
    /// limit, which is where the two differ — and where `>=` would offer a
    /// **Load next** that opens on nothing.
    #[tokio::test]
    async fn a_last_page_that_exactly_fills_the_limit_says_there_is_nothing_after_it() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        for number in 1..=4 {
            store.upsert_pr(&pr(1, number, 10)).await.unwrap();
        }
        let first = store
            .list_prs(
                INSTALLATION,
                &PrFilter::default(),
                Page {
                    limit: 2,
                    after: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            first.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [4, 3]
        );
        assert!(
            first.next_cursor.is_some(),
            "two rows are left, so there is a page after this one"
        );

        let second = store
            .list_prs(
                INSTALLATION,
                &PrFilter::default(),
                Page {
                    limit: 2,
                    after: first.next_cursor,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            second.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [2, 1]
        );
        assert!(
            second.next_cursor.is_none(),
            "the rows ran out on the limit, so nothing follows this page"
        );
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
            .list_prs(INSTALLATION, &PrFilter::default(), Page::default())
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
                .list_prs(INSTALLATION, &PrFilter::default(), Page { limit: 1, after })
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
                    INSTALLATION,
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
                INSTALLATION,
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
                    INSTALLATION,
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

    /// A scoped, paged read is three appends into one parameter vector —
    /// the filter's own parameters, then the installation scope, then the
    /// page limit — and every `?N` the statement renders is a position in
    /// that vector. `filter_sql`'s own tests pin its numbering; that the
    /// scope and the limit land where the rendered SQL says they do is
    /// pinned only here.
    #[test]
    fn a_scoped_page_numbers_the_filter_then_the_scope_then_the_limit() {
        let filter = PrFilter {
            repos: vec!["acme/api".to_owned()],
            update_types: vec![UpdateType::Minor],
            labels: vec!["rust".to_owned()],
            ..Default::default()
        };
        let cursor = PageCursor {
            updated_at: 500,
            id: "1#7".to_owned(),
        };

        let mut scoped = ScopedFilter::new(&filter, Some(&cursor), 10_000, INSTALLATION).unwrap();
        let sql = page_sql(&mut scoped, 51).unwrap();

        assert_eq!(
            sql,
            format!(
                "{} {} ORDER BY p.updated_at DESC, p.id DESC LIMIT ?7",
                select_pr_sql(),
                [
                    "WHERE p.repository_id IN (SELECT repository_id FROM repositories WHERE owner || '/' || repo IN (?1))",
                    "p.update_type IN (?2)",
                    "EXISTS (SELECT 1 FROM json_each(p.labels) l WHERE l.value = ?3)",
                    "(p.updated_at < ?4 OR (p.updated_at = ?4 AND p.id < ?5))",
                    "p.repository_id IN (SELECT repository_id FROM repositories WHERE installation_id = ?6)",
                ]
                .join(" AND ")
            )
        );
        assert_eq!(
            scoped.into_params(),
            [
                Value::Text("acme/api".to_owned()),
                Value::Text("minor".to_owned()),
                Value::Text("rust".to_owned()),
                Value::Integer(500),
                Value::Text("1#7".to_owned()),
                integer(INSTALLATION).unwrap(),
                Value::Integer(51),
            ]
        );
    }

    /// The scope predicate is spliced wherever the clause goes, including
    /// into the repository facet's derived table — which sits under a query
    /// that has already bound `r` to a *different* `repositories` row. So it
    /// names only `p`; an `r.` here would silently resolve to that outer row.
    #[test]
    fn the_installation_scope_predicate_names_no_repositories_alias() {
        let scoped = ScopedFilter::new(&PrFilter::default(), None, 10_000, INSTALLATION).unwrap();

        assert_eq!(
            scoped.where_sql(),
            "WHERE p.repository_id IN (SELECT repository_id FROM repositories WHERE installation_id = ?1)"
        );
        assert!(!scoped.where_sql().contains("r."), "{}", scoped.where_sql());
    }

    #[tokio::test]
    async fn the_dependency_filter_is_served_by_its_index() {
        let (_directory, store) = test_store().await;
        let filter = PrFilter {
            dependency: Some("serde".to_owned()),
            ..Default::default()
        };
        // The statements as `list_prs` assembles them, tenancy predicate
        // and all, so the scope cannot cost the filter its index unnoticed.
        let mut scoped = ScopedFilter::new(&filter, None, unix_seconds(), INSTALLATION).unwrap();
        let connection = store.connection().await;

        for sql in [
            count_sql(scoped.where_sql()),
            page_sql(&mut scoped, 51).unwrap(),
        ] {
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
    async fn last_synced_at_is_the_newest_sync_or_absent_when_empty() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        let empty = store
            .dashboard_summary(INSTALLATION, &PrFilter::default())
            .await
            .unwrap();
        assert_eq!(empty.last_synced_at, None);

        store.upsert_pr(&pr(1, 1, 300)).await.unwrap();
        store.upsert_pr(&pr(1, 2, 700)).await.unwrap();
        store.upsert_pr(&pr(1, 3, 500)).await.unwrap();

        let synced = store
            .dashboard_summary(
                INSTALLATION,
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
            .list_prs(INSTALLATION, &PrFilter::default(), Page::default())
            .await
            .unwrap();
        let page_selects = std::mem::take(&mut *selects.lock().unwrap());
        store
            .dashboard_summary(INSTALLATION, &PrFilter::default())
            .await
            .unwrap();
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
            .list_prs(INSTALLATION, &PrFilter::default(), Page::default())
            .await
            .unwrap();
        store
            .dashboard_summary(INSTALLATION, &PrFilter::default())
            .await
            .unwrap();
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
                INSTALLATION,
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

    /// `upsert_pr` writes the id `(repository_id, number)` derives, not the
    /// one the record carries, so no caller can put a row in the projection
    /// under an id that disagrees with the pull request it describes — the
    /// row would then be unreachable by its own key, and a second write for
    /// the same pull request would land beside it rather than over it.
    #[tokio::test]
    async fn a_row_is_stored_under_the_id_its_repository_and_number_derive_and_not_the_one_it_carries()
     {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        let mut mislabelled = pr(1, 7, 10);
        mislabelled.id = PrKey::new(1, 8).to_string();

        store.upsert_pr(&mislabelled).await.unwrap();

        assert_eq!(
            store.get_pr(&PrKey::new(1, 7)).await.unwrap(),
            Some(pr(1, 7, 10)),
            "the row is there under the key it actually has, id and all"
        );
        assert_eq!(
            store.get_pr(&PrKey::new(1, 8)).await.unwrap(),
            None,
            "and nowhere under the id it was handed"
        );
        // A second write of the same pull request goes over the first, so the
        // mislabelled id left no row of its own behind.
        store.upsert_pr(&pr(1, 7, 20)).await.unwrap();
        assert_eq!(
            store
                .list_prs(INSTALLATION, &PrFilter::default(), Page::default())
                .await
                .unwrap()
                .total,
            1
        );
    }

    #[tokio::test]
    async fn a_second_row_for_the_same_repository_and_number_is_rejected() {
        let (directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        // A row under some id of its own already holds (1, 7) — written
        // before ids were derived, or by hand. The index makes the pair as
        // unique as the derived id, so the store's own write for that pull
        // request is refused rather than becoming a second row for it.
        sidecar(&directory)
            .await
            .execute(
                r#"INSERT INTO pull_requests (
                    id, repository_id, owner, repo, number, title, html_url,
                    dependencies, update_type, head_sha, check_status, labels,
                    created_at, updated_at, synced_at
                ) VALUES ('rogue', 1, 'acme', 'repo-1', 7, 'Bump serde', '',
                    '[]', 'minor', 'sha-7', 'success', '[]', 0, 0, 10)"#,
                (),
            )
            .await
            .unwrap();

        let error = store.upsert_pr(&pr(1, 7, 10)).await.unwrap_err();

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
            None,
            "the refused row was not written under its derived id either"
        );
    }

    /// A check webhook names a commit, and the service asks the projection
    /// which of the repository's pull requests have it as their head. The
    /// answer is the repository's alone: another repository can hold a pull
    /// request at the same commit, and it is not this one's to sync.
    #[tokio::test]
    async fn the_pull_requests_at_a_head_are_those_of_the_repository_named_and_no_other() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_repo(&repo(2, 10)).await.unwrap();
        let at_head = |repository_id, number| PrRecord {
            head_sha: "abc123".to_owned(),
            ..pr(repository_id, number, 10)
        };
        // Two of repository 1's pull requests share the head, a third has one
        // of its own, and repository 2 has a pull request at the shared head.
        for record in [at_head(1, 1), at_head(1, 2), pr(1, 3, 10), at_head(2, 1)] {
            store.upsert_pr(&record).await.unwrap();
        }

        let mut found = store.prs_for_sha(1, "abc123").await.unwrap();

        found.sort_by_key(|record| record.number);
        assert_eq!(found, vec![at_head(1, 1), at_head(1, 2)]);
        assert_eq!(
            store.prs_for_sha(1, "no-such-head").await.unwrap(),
            Vec::new(),
            "a commit no pull request of the repository is at has nothing to sync"
        );
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
            .dashboard_summary(INSTALLATION, &PrFilter::default())
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

        // Each sync's repositories are read as its own deployment would,
        // since a summary is held to one installation.
        let mut survivors = Vec::new();
        for installation in [9_u64, 10] {
            let summary = store
                .dashboard_summary(installation, &PrFilter::default())
                .await
                .unwrap();
            survivors.extend(summary.facets.repositories.iter().map(|facet| {
                (
                    facet.repository.installation_id,
                    facet.repository.repository_id,
                )
            }));
        }
        survivors.sort_unstable();
        // Every round retires the previous round's repo, so exactly the last
        // one per installation remains.
        assert_eq!(survivors, [(9, 924), (10, 1024)]);
    }
}
