//! Versioned schema migrations. Every file under `migrations/` is embedded
//! at compile time and applied exactly once, oldest first, with the applied
//! versions recorded in `schema_migrations`.

use std::collections::BTreeSet;

use dependaboard_core::unix_seconds;
use libsql::{Connection, TransactionBehavior, Value};

use crate::{StoreError, integer};

struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// Every migration, oldest first. To add one, drop `NNNN_name.sql` into
/// `migrations/` and append it here; the registry test checks both agree.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        sql: include_str!("../../../migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "pull_request_constraints",
        sql: include_str!("../../../migrations/0002_pull_request_constraints.sql"),
    },
    Migration {
        version: 3,
        name: "repository_merge_method",
        sql: include_str!("../../../migrations/0003_repository_merge_method.sql"),
    },
    Migration {
        version: 4,
        name: "projection_revision",
        sql: include_str!("../../../migrations/0004_projection_revision.sql"),
    },
    Migration {
        version: 5,
        name: "batches",
        sql: include_str!("../../../migrations/0005_batches.sql"),
    },
    Migration {
        version: 6,
        name: "drop_owner_filter_index",
        sql: include_str!("../../../migrations/0006_drop_owner_filter_index.sql"),
    },
    Migration {
        version: 7,
        name: "pull_request_revision",
        sql: include_str!("../../../migrations/0007_pull_request_revision.sql"),
    },
    Migration {
        version: 8,
        name: "pull_request_retirements",
        sql: include_str!("../../../migrations/0008_pull_request_retirements.sql"),
    },
];

const CREATE_SCHEMA_MIGRATIONS: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    applied_at INTEGER NOT NULL
)";

/// Applies every registered migration the database behind `connection` has
/// not seen yet and returns the versions this call applied, in order.
///
/// A current database is left alone after one read. Each pending migration
/// runs in its own `BEGIN IMMEDIATE` transaction with the version re-checked
/// inside: when the web and Restate processes start together against a new
/// version, the second waits for the first's write lock and then finds the
/// version recorded, instead of both running the same DDL.
pub(crate) async fn apply(connection: &Connection) -> Result<Vec<u32>, StoreError> {
    connection.execute(CREATE_SCHEMA_MIGRATIONS, ()).await?;
    let recorded = recorded_versions(connection).await?;
    let mut applied = Vec::new();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| !recorded.contains(&migration.version))
    {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        if recorded_versions(&transaction)
            .await?
            .contains(&migration.version)
        {
            transaction.rollback().await?;
            continue;
        }
        transaction.execute_batch(migration.sql).await?;
        transaction
            .execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                vec![
                    Value::Integer(i64::from(migration.version)),
                    Value::Text(migration.name.to_owned()),
                    integer(unix_seconds())?,
                ],
            )
            .await?;
        transaction.commit().await?;
        applied.push(migration.version);
    }
    Ok(applied)
}

async fn recorded_versions(connection: &Connection) -> Result<BTreeSet<u32>, StoreError> {
    let mut rows = connection
        .query("SELECT version FROM schema_migrations", ())
        .await?;
    let mut versions = BTreeSet::new();
    while let Some(row) = rows.next().await? {
        versions
            .insert(u32::try_from(row.get::<i64>(0)?).map_err(|_| StoreError::IntegerOverflow)?);
    }
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use libsql::{Builder, Row};

    use super::*;

    /// A connection set up the way `LibSqlPrStore::connect` sets one up, so
    /// the DDL runs under the same foreign-key enforcement as in production.
    async fn connection() -> Connection {
        let connection = Builder::new_local(":memory:")
            .build()
            .await
            .unwrap()
            .connect()
            .unwrap();
        connection
            .execute("PRAGMA foreign_keys = ON", ())
            .await
            .unwrap();
        connection
    }

    async fn rows<T>(connection: &Connection, sql: &str, read: impl Fn(&Row) -> T) -> Vec<T> {
        let mut rows = connection.query(sql, ()).await.unwrap();
        let mut values = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            values.push(read(&row));
        }
        values
    }

    async fn recorded(connection: &Connection) -> Vec<(u32, String)> {
        rows(
            connection,
            "SELECT version, name FROM schema_migrations ORDER BY version",
            |row| {
                (
                    u32::try_from(row.get::<i64>(0).unwrap()).unwrap(),
                    row.get::<String>(1).unwrap(),
                )
            },
        )
        .await
    }

    /// The DDL text SQLite keeps for every table and index, which is what
    /// "the same schema" means: it captures constraints and collations too.
    async fn schema(connection: &Connection) -> Vec<(String, String, String)> {
        rows(
            connection,
            "SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name",
            |row| {
                (
                    row.get::<String>(0).unwrap(),
                    row.get::<String>(1).unwrap(),
                    row.get::<String>(2).unwrap(),
                )
            },
        )
        .await
    }

    async fn count(connection: &Connection, table: &str) -> i64 {
        rows(
            connection,
            &format!("SELECT COUNT(*) FROM {table}"),
            |row| row.get::<i64>(0).unwrap(),
        )
        .await[0]
    }

    #[tokio::test]
    async fn applies_every_registered_migration_once_in_order_and_rerunning_is_a_no_op() {
        let connection = connection().await;

        let first = apply(&connection).await.unwrap();
        let again = apply(&connection).await.unwrap();

        let registry = MIGRATIONS
            .iter()
            .map(|migration| (migration.version, migration.name.to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(
            first,
            registry
                .iter()
                .map(|(version, _)| *version)
                .collect::<Vec<_>>()
        );
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(recorded(&connection).await, registry);
    }

    #[test]
    fn every_migration_file_is_registered_under_its_own_name_with_contiguous_versions() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
        let mut files = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        files.sort_unstable();

        let registered = MIGRATIONS
            .iter()
            .map(|migration| format!("{:04}_{}.sql", migration.version, migration.name))
            .collect::<Vec<_>>();
        let versions = MIGRATIONS
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>();

        assert_eq!(registered, files);
        assert_eq!(versions, (1..=versions.len() as u32).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_database_from_before_versioning_is_adopted_and_converges_on_the_fresh_schema() {
        // Before this runner existed, connecting ran 0001 verbatim on every
        // start and nothing recorded that it had. Such a database has the
        // tables, has data, and has no schema_migrations.
        let legacy = connection().await;
        legacy.execute_batch(MIGRATIONS[0].sql).await.unwrap();
        legacy
            .execute_batch(
                "INSERT INTO repositories VALUES (1, 9, 'acme', 'api', 10);
                 INSERT INTO pull_requests (
                    id, repository_id, owner, repo, number, title, html_url, dependency,
                    update_type, head_sha, check_status, created_at, updated_at, synced_at
                 ) VALUES ('1#7', 1, 'acme', 'api', 7, 'Bump Serde', 'https://x', 'Serde',
                    'minor', 'sha', 'success', 0, 0, 10);",
            )
            .await
            .unwrap();
        let fresh = connection().await;

        let adopted = apply(&legacy).await.unwrap();
        let created = apply(&fresh).await.unwrap();

        assert_eq!(adopted, created);
        assert_eq!(schema(&legacy).await, schema(&fresh).await);
        assert_eq!(count(&legacy, "pull_requests").await, 1);
        assert_eq!(count(&legacy, "repositories").await, 1);
    }

    #[tokio::test]
    async fn the_pull_request_indexes_are_the_ones_the_queries_use_and_idx_pr_number_is_unique() {
        let connection = connection().await;
        apply(&connection).await.unwrap();

        // (name, unique) per PRAGMA index_list.
        let mut indexes = rows(&connection, "PRAGMA index_list(pull_requests)", |row| {
            (
                row.get::<String>(1).unwrap(),
                row.get::<i64>(2).unwrap() == 1,
            )
        })
        .await;
        indexes.sort_unstable();

        // idx_pr_repo (repository_id) is absent: idx_pr_sha and idx_pr_number
        // both start with repository_id and cover it. idx_pr_filter is gone
        // with the owner filter that alone could use it. The autoindex is the
        // TEXT PRIMARY KEY on id.
        assert_eq!(
            indexes,
            [
                ("idx_pr_dependency".to_owned(), false),
                ("idx_pr_number".to_owned(), true),
                ("idx_pr_order".to_owned(), false),
                ("idx_pr_sha".to_owned(), false),
                ("sqlite_autoindex_pull_requests_1".to_owned(), true),
            ]
        );
    }
}
