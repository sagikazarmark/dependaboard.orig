//! The counts that frame the dashboard's rows: for each facet, how many pull
//! requests each of its values would leave, counted within every other
//! filter but not the facet's own, so the sidebar still offers the value that
//! would widen the view.

use std::{collections::BTreeMap, str::FromStr};

use dependaboard_core::{FacetCounts, LabelFacet, PrFilter, RepoFacet};
use libsql::Value;

use crate::{
    StoreError,
    filter::{Facet, filter_sql, without_facet},
    repo_from_row, stored_enum, unsigned,
};

pub(crate) async fn facet_counts(
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

#[cfg(test)]
mod tests {
    use dependaboard_core::{
        CheckStatus, DashboardSummary, Page, PrRecord, RepoRecord, UpdateType,
    };
    use tempfile::TempDir;

    use crate::{
        LibSqlPrStore, ProjectionReader, ProjectionWriter,
        test_support::{pr, repo, test_store},
    };

    use super::*;

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
}
