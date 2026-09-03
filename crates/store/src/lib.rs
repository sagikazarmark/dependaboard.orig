use std::{collections::BTreeMap, env, path::Path, str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use dependaboard_core::{
    CheckStatus, CursorError, DashboardPage, FacetCounts, LabelFacet, Mergeable, Page, PageCursor,
    PrFilter, PrKey, PrRecord, RepoRecord, UpdateType, unix_seconds,
};
use libsql::{Builder, Row, Value};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::sync::{Mutex, MutexGuard};

mod filter;
mod migrations;

use filter::filter_sql;

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
    async fn retain_prs(
        &self,
        repository_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<u64, StoreError>;
    async fn list_prs(&self, filter: &PrFilter, page: Page) -> Result<DashboardPage, StoreError>;
    async fn prs_for_sha(&self, repository_id: u64, sha: &str)
    -> Result<Vec<PrRecord>, StoreError>;
    async fn upsert_repo(&self, repo: &RepoRecord) -> Result<(), StoreError>;
    async fn replace_installation_repos(
        &self,
        installation_id: u64,
        repos: &[RepoRecord],
        synced_before: u64,
    ) -> Result<u64, StoreError>;
    async fn retain_repos(
        &self,
        installation_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<u64, StoreError>;
    async fn purge_installation(&self, installation_id: u64) -> Result<u64, StoreError>;
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
    ) -> Result<u64, StoreError> {
        let connection = self.connection().await;
        retain_prs_on(&connection, repository_id, live, synced_before).await
    }

    async fn list_prs(&self, filter: &PrFilter, page: Page) -> Result<DashboardPage, StoreError> {
        let connection = self.connection().await;
        let now = unix_seconds();
        let (where_sql, params) = filter_sql(filter, None, now)?;
        let total = scalar_u64(&connection, &count_sql(&where_sql), params).await?;

        let cursor = page.after.as_deref().map(PageCursor::decode).transpose()?;
        let (page_where, mut page_params) = filter_sql(filter, cursor.as_ref(), now)?;
        let limit = page.normalized_limit() as usize;
        let limit_index = page_params.len() + 1;
        page_params.push(integer((limit + 1) as u64)?);
        let mut query_rows = connection
            .query(&page_sql(&page_where, limit_index), page_params)
            .await?;
        let mut rows = Vec::with_capacity(limit + 1);
        while let Some(row) = query_rows.next().await? {
            rows.push(pr_from_row(row)?);
        }
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = has_more.then(|| rows.last()).flatten().map(|record| {
            PageCursor {
                updated_at: record.updated_at,
                id: record.id.clone(),
            }
            .encode()
        });

        let repositories = list_repositories(&connection).await?;
        let facets = facet_counts(&connection).await?;
        let last_synced_at = scalar_optional_u64(
            &connection,
            "SELECT MAX(synced_at) FROM pull_requests",
            Vec::new(),
        )
        .await?;
        Ok(DashboardPage {
            rows,
            total,
            next_cursor,
            repositories,
            facets,
            last_synced_at,
        })
    }

    async fn prs_for_sha(
        &self,
        repository_id: u64,
        sha: &str,
    ) -> Result<Vec<PrRecord>, StoreError> {
        let connection = self.connection().await;
        let mut rows = connection
            .query(
                &format!(
                    "{} WHERE p.repository_id = ?1 AND p.head_sha = ?2",
                    select_pr_sql()
                ),
                vec![integer(repository_id)?, Value::Text(sha.to_owned())],
            )
            .await?;
        let mut records = Vec::new();
        while let Some(row) = rows.next().await? {
            records.push(pr_from_row(row)?);
        }
        Ok(records)
    }

    async fn upsert_repo(&self, repo: &RepoRecord) -> Result<(), StoreError> {
        let connection = self.connection().await;
        upsert_repo_on(&connection, repo).await
    }

    async fn replace_installation_repos(
        &self,
        installation_id: u64,
        repos: &[RepoRecord],
        synced_before: u64,
    ) -> Result<u64, StoreError> {
        let connection = self.connection().await;
        let transaction = connection.transaction().await?;
        for repo in repos {
            upsert_repo_on(&transaction, repo).await?;
        }
        let live = repos
            .iter()
            .map(|repo| repo.repository_id)
            .collect::<Vec<_>>();
        let deleted = retain_repos_on(&transaction, installation_id, &live, synced_before).await?;
        transaction.commit().await?;
        Ok(deleted)
    }

    async fn retain_repos(
        &self,
        installation_id: u64,
        live: &[u64],
        synced_before: u64,
    ) -> Result<u64, StoreError> {
        let connection = self.connection().await;
        retain_repos_on(&connection, installation_id, live, synced_before).await
    }

    async fn purge_installation(&self, installation_id: u64) -> Result<u64, StoreError> {
        Ok(self
            .connection()
            .await
            .execute(
                "DELETE FROM repositories WHERE installation_id = ?1",
                vec![integer(installation_id)?],
            )
            .await?)
    }
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
    let update_type: String = row.get(12)?;
    let check_status: String = row.get(14)?;
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
        update_type: UpdateType::from_str(&update_type)
            .map_err(|_| StoreError::CorruptEnum(update_type))?,
        head_sha: row.get(13)?,
        check_status: CheckStatus::from_str(&check_status)
            .map_err(|_| StoreError::CorruptEnum(check_status))?,
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

async fn list_repositories(connection: &libsql::Connection) -> Result<Vec<RepoRecord>, StoreError> {
    let mut rows = connection
        .query(
            "SELECT repository_id, installation_id, owner, repo, synced_at FROM repositories ORDER BY owner, repo",
            (),
        )
        .await?;
    let mut repositories = Vec::new();
    while let Some(row) = rows.next().await? {
        repositories.push(RepoRecord {
            repository_id: unsigned(row.get::<i64>(0)?)?,
            installation_id: unsigned(row.get::<i64>(1)?)?,
            owner: row.get(2)?,
            repo: row.get(3)?,
            synced_at: unsigned(row.get::<i64>(4)?)?,
        });
    }
    Ok(repositories)
}

async fn facet_counts(connection: &libsql::Connection) -> Result<FacetCounts, StoreError> {
    Ok(FacetCounts {
        checks: enum_counts(
            connection,
            "SELECT check_status, COUNT(*) FROM pull_requests GROUP BY check_status",
        )
        .await?,
        update_types: enum_counts(
            connection,
            "SELECT update_type, COUNT(*) FROM pull_requests GROUP BY update_type",
        )
        .await?,
        // Ties fall back to case-insensitive name order; the GROUP BY stays
        // exact because the label filter matches labels byte for byte.
        labels: grouped_counts(
            connection,
            "SELECT value, COUNT(*) FROM pull_requests, json_each(labels) GROUP BY value ORDER BY COUNT(*) DESC, value COLLATE NOCASE",
        )
        .await?
        .into_iter()
        .map(|(label, count)| LabelFacet { label, count })
        .collect(),
    })
}

/// Groups by a column that persists an enum's `Display` form and keys the
/// result by the parsed enum, so callers never see the raw text. Unrecognised
/// text is corrupt data, exactly as it is when reading a row.
async fn enum_counts<T>(
    connection: &libsql::Connection,
    sql: &str,
) -> Result<BTreeMap<T, u64>, StoreError>
where
    T: FromStr + Ord,
{
    grouped_counts(connection, sql)
        .await?
        .into_iter()
        .map(|(key, count)| {
            T::from_str(&key)
                .map(|key| (key, count))
                .map_err(|_| StoreError::CorruptEnum(key))
        })
        .collect()
}

/// Runs a `SELECT key, COUNT(*)` query and returns the rows in the order the
/// database produced them, so an `ORDER BY` in the query survives.
async fn grouped_counts(
    connection: &libsql::Connection,
    sql: &str,
) -> Result<Vec<(String, u64)>, StoreError> {
    let mut rows = connection.query(sql, ()).await?;
    let mut counts = Vec::new();
    while let Some(row) = rows.next().await? {
        counts.push((row.get(0)?, unsigned(row.get::<i64>(1)?)?));
    }
    Ok(counts)
}

async fn scalar_u64(
    connection: &libsql::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<u64, StoreError> {
    let mut rows = connection.query(sql, params).await?;
    let row = rows.next().await?.ok_or(StoreError::MissingScalar)?;
    unsigned(row.get::<i64>(0)?)
}

async fn scalar_optional_u64(
    connection: &libsql::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Option<u64>, StoreError> {
    let mut rows = connection.query(sql, params).await?;
    let row = rows.next().await?.ok_or(StoreError::MissingScalar)?;
    row.get::<Option<i64>>(0)?.map(unsigned).transpose()
}

async fn upsert_repo_on(
    connection: &libsql::Connection,
    repo: &RepoRecord,
) -> Result<(), StoreError> {
    connection
        .execute(
            r#"INSERT INTO repositories (
                repository_id, installation_id, owner, repo, synced_at
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(repository_id) DO UPDATE SET
                installation_id = excluded.installation_id,
                owner = excluded.owner,
                repo = excluded.repo,
                synced_at = excluded.synced_at"#,
            vec![
                integer(repo.repository_id)?,
                integer(repo.installation_id)?,
                Value::Text(repo.owner.clone()),
                Value::Text(repo.repo.clone()),
                integer(repo.synced_at)?,
            ],
        )
        .await?;
    Ok(())
}

async fn retain_prs_on(
    connection: &libsql::Connection,
    repository_id: u64,
    live: &[u64],
    synced_before: u64,
) -> Result<u64, StoreError> {
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
    Ok(connection
        .execute(
            &format!(
                "DELETE FROM pull_requests WHERE repository_id = ?1 AND synced_at < ?2{live_clause}"
            ),
            params,
        )
        .await?)
}

async fn retain_repos_on(
    connection: &libsql::Connection,
    installation_id: u64,
    live: &[u64],
    synced_before: u64,
) -> Result<u64, StoreError> {
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
    Ok(connection
        .execute(
            &format!(
                "DELETE FROM repositories WHERE installation_id = ?1 AND synced_at < ?2{live_clause}"
            ),
            params,
        )
        .await?)
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
        libsql::Error::RemoteSqliteFailure(code, extended_code, _) => {
            retryable_sqlite_code(*code) || retryable_sqlite_code(*extended_code)
        }
        libsql::Error::ConnectionFailed(_) | libsql::Error::WalConflict => true,
        _ => false,
    }
}

fn retryable_sqlite_code(code: i32) -> bool {
    // Extended SQLite result codes retain the primary result in the low byte.
    matches!(code & 0xff, 5 | 6 | 10) || matches!(code, 787 | 1555 | 2067) // foreign key, primary key, unique
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{CheckStatus, DependencyUpdate, UpdateType};
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

        let result = store
            .list_prs(&PrFilter::default(), Page::default())
            .await
            .unwrap();

        let ranked = result
            .facets
            .labels
            .iter()
            .map(|facet| (facet.label.as_str(), facet.count))
            .collect::<Vec<_>>();
        assert_eq!(
            ranked,
            [("rust", 4), ("go", 2), ("Security", 2), ("dependencies", 1)]
        );
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

        let result = store
            .list_prs(&PrFilter::default(), Page::default())
            .await
            .unwrap();

        assert_eq!(
            result.facets.checks,
            BTreeMap::from([(CheckStatus::Success, 3), (CheckStatus::Failure, 1)])
        );
        assert_eq!(
            result.facets.update_types,
            BTreeMap::from([(UpdateType::Minor, 3), (UpdateType::Major, 1)])
        );
    }

    #[tokio::test]
    async fn facets_and_repositories_span_the_whole_read_model_while_rows_honor_the_filter() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        store.upsert_repo(&repo(2, 10)).await.unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();
        let mut failing = pr(2, 2, 10);
        failing.check_status = CheckStatus::Failure;
        failing.labels = vec!["go".to_owned()];
        store.upsert_pr(&failing).await.unwrap();

        let result = store
            .list_prs(
                &PrFilter {
                    check_statuses: vec![CheckStatus::Failure],
                    ..Default::default()
                },
                Page::default(),
            )
            .await
            .unwrap();

        assert_eq!(result.total, 1);
        assert_eq!(
            result.rows.iter().map(|pr| pr.number).collect::<Vec<_>>(),
            [2]
        );
        // The sidebar must keep offering the filters that would widen the view.
        assert_eq!(
            result.facets.checks,
            BTreeMap::from([(CheckStatus::Success, 1), (CheckStatus::Failure, 1)])
        );
        assert_eq!(
            result
                .facets
                .labels
                .iter()
                .map(|facet| (facet.label.as_str(), facet.count))
                .collect::<Vec<_>>(),
            [("dependencies", 1), ("go", 1), ("rust", 1)]
        );
        assert_eq!(
            result
                .repositories
                .iter()
                .map(|repo| repo.repository_id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[tokio::test]
    async fn last_synced_at_is_the_newest_sync_or_absent_when_empty() {
        let (_directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();
        let empty = store
            .list_prs(&PrFilter::default(), Page::default())
            .await
            .unwrap();
        assert_eq!(empty.last_synced_at, None);

        store.upsert_pr(&pr(1, 1, 300)).await.unwrap();
        store.upsert_pr(&pr(1, 2, 700)).await.unwrap();
        store.upsert_pr(&pr(1, 3, 500)).await.unwrap();

        let synced = store
            .list_prs(
                // The freshness stamp is for the whole projection, not the
                // rows the filter happens to leave visible.
                &PrFilter {
                    query: Some("no such pull request".to_owned()),
                    ..Default::default()
                },
                Page::default(),
            )
            .await
            .unwrap();
        assert_eq!(synced.total, 0);
        assert_eq!(synced.last_synced_at, Some(700));
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
            store.get_pr(&PrKey::new(1, 7)).await.unwrap(),
            Some(pr(1, 7, 10))
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
    async fn replacing_installation_repos_removes_stale_repos_in_one_transaction() {
        let (_directory, store) = test_store().await;
        store
            .replace_installation_repos(9, &[repo(1, 10), repo(2, 10)], 20)
            .await
            .unwrap();
        store.upsert_pr(&pr(1, 1, 10)).await.unwrap();

        let deleted = store
            .replace_installation_repos(9, &[repo(2, 30)], 20)
            .await
            .unwrap();

        assert_eq!(deleted, 1);
        assert!(store.get_pr(&PrKey::new(1, 1)).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writes_wait_for_a_writer_in_another_process() {
        let (directory, store) = test_store().await;
        store.upsert_repo(&repo(1, 10)).await.unwrap();

        let other_process = sidecar(&directory).await;
        let transaction = other_process.transaction().await.unwrap();
        upsert_repo_on(&transaction, &repo(2, 10)).await.unwrap();

        let write = tokio::spawn(async move { store.upsert_pr(&pr(1, 1, 10)).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        transaction.commit().await.unwrap();

        write.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connecting_to_a_current_database_does_not_wait_for_another_writer() {
        let (directory, _store) = test_store().await;
        // The other process is mid-write for longer than the busy timeout.
        // A start-up that only needs to read the schema version must not
        // queue behind it, let alone fail with SQLITE_BUSY.
        let other_process = sidecar(&directory).await;
        let _transaction = other_process
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        LibSqlPrStore::connect(&StoreConfig::local(database_path(&directory)))
            .await
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "connect waited {:?} for the other writer",
            started.elapsed()
        );
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

        let page = store
            .list_prs(&PrFilter::default(), Page::default())
            .await
            .unwrap();
        let mut survivors = page
            .repositories
            .iter()
            .map(|repo| (repo.installation_id, repo.repository_id))
            .collect::<Vec<_>>();
        survivors.sort_unstable();
        // Every round retires the previous round's repo, so exactly the last
        // one per installation remains.
        assert_eq!(survivors, [(9, 924), (10, 1024)]);
    }

    #[test]
    fn classifies_structured_sqlite_contention_and_constraints_as_retryable() {
        for code in [5, 6, 10, 5 | (2 << 8), 787, 1555, 2067] {
            let error = StoreError::Database(libsql::Error::SqliteFailure(code, "busy".into()));
            assert_eq!(error.class(), StoreErrorClass::Retryable);
        }
        let remote = StoreError::Database(libsql::Error::RemoteSqliteFailure(
            1,
            19 | (8 << 8),
            "constraint".into(),
        ));
        assert_eq!(remote.class(), StoreErrorClass::Retryable);
    }

    #[test]
    fn classifies_unknown_and_data_errors_as_terminal() {
        let database = StoreError::Database(libsql::Error::Misuse("bad call".into()));
        assert_eq!(database.class(), StoreErrorClass::Terminal);
        let generic_constraint =
            StoreError::Database(libsql::Error::SqliteFailure(19, "constraint".into()));
        assert_eq!(generic_constraint.class(), StoreErrorClass::Terminal);
        assert_eq!(
            StoreError::IntegerOverflow.class(),
            StoreErrorClass::Terminal
        );
        assert_eq!(StoreError::MissingScalar.class(), StoreErrorClass::Terminal);
    }
}
