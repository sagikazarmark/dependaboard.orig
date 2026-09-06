//! Translates a [`PrFilter`] into a SQL `WHERE` clause over `pull_requests p`
//! plus the positional parameters it binds.

use dependaboard_core::{CheckStatus, Mergeable, PageCursor, PrFilter, STALE_AFTER, UpdateType};
use libsql::Value;

use crate::{StoreError, integer};

/// Builds the `WHERE` clause for `filter`, optionally continuing after
/// `cursor` in `(updated_at DESC, id DESC)` order. `now` (Unix seconds)
/// anchors the "stale" predicate of `needs_attention`, so the output depends
/// only on its arguments.
pub(crate) fn filter_sql(
    filter: &PrFilter,
    cursor: Option<&PageCursor>,
    now: u64,
) -> Result<(String, Vec<Value>), StoreError> {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    let mut bind = |value: Value| {
        params.push(value);
        format!("?{}", params.len())
    };
    if let Some(query) = filter
        .query
        .as_ref()
        .filter(|query| !query.trim().is_empty())
    {
        let binding = bind(Value::Text(format!(
            "%{}%",
            escape_like(&query.trim().to_ascii_lowercase())
        )));
        clauses.push(format!(
            r"(LOWER(p.owner || '/' || p.repo || ' ' || p.title) LIKE {binding} ESCAPE '\' OR EXISTS (SELECT 1 FROM json_each(p.dependencies) d WHERE LOWER(json_extract(d.value, '$.name')) LIKE {binding} ESCAPE '\'))"
        ));
    }
    if let Some(owner) = filter.owner.as_ref().filter(|owner| !owner.is_empty()) {
        let binding = bind(Value::Text(owner.clone()));
        clauses.push(format!("p.owner = {binding}"));
    }
    if !filter.repos.is_empty() {
        let bindings = filter
            .repos
            .iter()
            .map(|repo| bind(Value::Text(repo.clone())))
            .collect::<Vec<_>>()
            .join(", ");
        clauses.push(format!("(p.owner || '/' || p.repo) IN ({bindings})"));
    }
    if !filter.update_types.is_empty() {
        let bindings = filter
            .update_types
            .iter()
            .map(|update_type| bind(Value::Text(update_type.to_string())))
            .collect::<Vec<_>>()
            .join(", ");
        clauses.push(format!("p.update_type IN ({bindings})"));
    }
    if !filter.check_statuses.is_empty() {
        let bindings = filter
            .check_statuses
            .iter()
            .map(|status| bind(Value::Text(status.to_string())))
            .collect::<Vec<_>>()
            .join(", ");
        clauses.push(format!("p.check_status IN ({bindings})"));
    }
    for label in &filter.labels {
        let binding = bind(Value::Text(label.clone()));
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM json_each(p.labels) l WHERE l.value = {binding})"
        ));
    }
    if let Some(dependency) = filter
        .dependency
        .as_ref()
        .filter(|dependency| !dependency.trim().is_empty())
    {
        // `dependency` is COLLATE NOCASE, so the plain comparison is both
        // case-insensitive and served by idx_pr_dependency. A PR either
        // names its single dependency there or, for a grouped update, is
        // NULL there and lists them all in `dependencies`; scoping the JSON
        // branch to `IS NULL` keeps that branch on the index as well, so
        // SQLite runs the OR as two index searches instead of a scan.
        let binding = bind(Value::Text(dependency.trim().to_owned()));
        clauses.push(format!(
            "(p.dependency = {binding} OR (p.dependency IS NULL AND EXISTS (SELECT 1 FROM json_each(p.dependencies) d WHERE json_extract(d.value, '$.name') = {binding} COLLATE NOCASE)))"
        ));
    }
    if filter.needs_attention {
        // Every enum here binds its `Display` form, like the facet filters
        // above, so a renamed variant cannot leave a stale literal behind.
        let failing = [CheckStatus::Failure, CheckStatus::None]
            .into_iter()
            .map(|status| bind(Value::Text(status.to_string())))
            .collect::<Vec<_>>()
            .join(", ");
        let conflicting = Mergeable::ALL
            .into_iter()
            .filter(|state| state.is_conflicting())
            .map(|state| bind(Value::Text(state.to_string())))
            .collect::<Vec<_>>()
            .join(", ");
        let major = bind(Value::Text(UpdateType::Major.to_string()));
        let stale_before = bind(integer(now.saturating_sub(STALE_AFTER.as_secs()))?);
        clauses.push(format!(
            "(p.check_status IN ({failing}) OR p.mergeable IN ({conflicting}) OR p.update_type = {major} OR p.synced_at < {stale_before})"
        ));
    }
    if let Some(cursor) = cursor {
        let updated_at = bind(integer(cursor.updated_at)?);
        let id = bind(Value::Text(cursor.id.clone()));
        clauses.push(format!(
            "(p.updated_at < {updated_at} OR (p.updated_at = {updated_at} AND p.id < {id}))"
        ));
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    Ok((sql, params))
}

/// Escapes the `LIKE` metacharacters in user text so they match themselves.
/// Pair with `ESCAPE '\'` on the clause.
fn escape_like(text: &str) -> String {
    text.replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_")
}

/// A sidebar facet: one dimension of [`PrFilter`] that is counted rather
/// than merely applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Facet {
    Checks,
    UpdateTypes,
    Labels,
    Repositories,
}

/// `filter` with `facet`'s own dimension dropped, so its counts reflect every
/// other filter in force and a value already chosen keeps showing the
/// alternatives.
pub(crate) fn without_facet(filter: &PrFilter, facet: Facet) -> PrFilter {
    let mut scoped = filter.clone();
    match facet {
        Facet::Checks => scoped.check_statuses.clear(),
        Facet::UpdateTypes => scoped.update_types.clear(),
        Facet::Labels => scoped.labels.clear(),
        Facet::Repositories => scoped.repos.clear(),
    }
    scoped
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 10_000;

    fn cursor() -> PageCursor {
        PageCursor {
            updated_at: 500,
            id: "1#7".to_owned(),
        }
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    #[test]
    fn empty_filter_has_no_where_clause() {
        let (sql, params) = filter_sql(&PrFilter::default(), None, NOW).unwrap();

        assert_eq!(sql, "");
        assert!(params.is_empty());
    }

    #[test]
    fn blank_text_fields_are_ignored() {
        let filter = PrFilter {
            query: Some("   ".to_owned()),
            owner: Some(String::new()),
            dependency: Some("\t".to_owned()),
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(sql, "");
        assert!(params.is_empty());
    }

    #[test]
    fn cursor_alone_opens_the_where_clause() {
        let (sql, params) = filter_sql(&PrFilter::default(), Some(&cursor()), NOW).unwrap();

        assert_eq!(
            sql,
            "WHERE (p.updated_at < ?1 OR (p.updated_at = ?1 AND p.id < ?2))"
        );
        assert_eq!(params, [Value::Integer(500), text("1#7")]);
    }

    #[test]
    fn cursor_binds_after_the_filter_parameters() {
        let filter = PrFilter {
            owner: Some("acme".to_owned()),
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, Some(&cursor()), NOW).unwrap();

        assert_eq!(
            sql,
            "WHERE p.owner = ?1 AND (p.updated_at < ?2 OR (p.updated_at = ?2 AND p.id < ?3))"
        );
        assert_eq!(params, [text("acme"), Value::Integer(500), text("1#7")]);
    }

    #[test]
    fn owner_matches_exactly() {
        let filter = PrFilter {
            owner: Some("Acme".to_owned()),
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(sql, "WHERE p.owner = ?1");
        assert_eq!(params, [text("Acme")]);
    }

    #[test]
    fn repos_match_the_owner_slash_repo_full_name() {
        let filter = PrFilter {
            repos: vec!["acme/api".to_owned(), "acme/web".to_owned()],
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(sql, "WHERE (p.owner || '/' || p.repo) IN (?1, ?2)");
        assert_eq!(params, [text("acme/api"), text("acme/web")]);
    }

    #[test]
    fn update_types_bind_their_persisted_names() {
        let filter = PrFilter {
            update_types: vec![UpdateType::Major, UpdateType::Unknown],
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(sql, "WHERE p.update_type IN (?1, ?2)");
        assert_eq!(params, [text("major"), text("unknown")]);
    }

    #[test]
    fn check_statuses_bind_their_persisted_names() {
        let filter = PrFilter {
            check_statuses: vec![CheckStatus::Failure, CheckStatus::None],
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(sql, "WHERE p.check_status IN (?1, ?2)");
        assert_eq!(params, [text("failure"), text("none")]);
    }

    #[test]
    fn every_label_must_be_present_byte_for_byte() {
        let filter = PrFilter {
            labels: vec!["rust".to_owned(), "Security".to_owned()],
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(
            sql,
            "WHERE EXISTS (SELECT 1 FROM json_each(p.labels) l WHERE l.value = ?1) AND EXISTS (SELECT 1 FROM json_each(p.labels) l WHERE l.value = ?2)"
        );
        assert_eq!(params, [text("rust"), text("Security")]);
    }

    #[test]
    fn dependency_matches_the_primary_or_any_grouped_dependency_case_insensitively() {
        let filter = PrFilter {
            dependency: Some(" Tokio ".to_owned()),
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(
            sql,
            "WHERE (p.dependency = ?1 OR (p.dependency IS NULL AND EXISTS (SELECT 1 FROM json_each(p.dependencies) d WHERE json_extract(d.value, '$.name') = ?1 COLLATE NOCASE)))"
        );
        assert_eq!(params, [text("Tokio")]);
    }

    #[test]
    fn search_text_escapes_like_wildcards_and_the_escape_character() {
        let filter = PrFilter {
            query: Some(r"  50%_Off\Now  ".to_owned()),
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        assert_eq!(
            sql,
            r"WHERE (LOWER(p.owner || '/' || p.repo || ' ' || p.title) LIKE ?1 ESCAPE '\' OR EXISTS (SELECT 1 FROM json_each(p.dependencies) d WHERE LOWER(json_extract(d.value, '$.name')) LIKE ?1 ESCAPE '\'))"
        );
        assert_eq!(params, [text(r"%50\%\_off\\now%")]);
    }

    /// Every enum the view compares against is bound in its persisted form,
    /// the same way the facet filters bind theirs, so renaming a variant
    /// cannot leave a stale literal behind in the SQL.
    #[test]
    fn needs_attention_binds_its_enum_values_and_the_stale_threshold_from_now() {
        let filter = PrFilter {
            needs_attention: true,
            ..Default::default()
        };

        let (sql, params) = filter_sql(&filter, None, NOW).unwrap();

        // STALE_AFTER is 45 minutes, so 10 000 - 2 700.
        assert_eq!(
            sql,
            "WHERE (p.check_status IN (?1, ?2) OR p.mergeable IN (?3) OR p.update_type = ?4 OR p.synced_at < ?5)"
        );
        assert_eq!(
            params,
            [
                text("failure"),
                text("none"),
                text("dirty"),
                text("major"),
                Value::Integer(7_300)
            ]
        );
    }

    /// A facet counts against every other dimension of the filter, so
    /// dropping its own leaves the rest, the free-text query and the view
    /// included, exactly as they were.
    #[test]
    fn dropping_a_facets_dimension_leaves_every_other_field_alone() {
        let filter = PrFilter {
            query: Some("serde".to_owned()),
            owner: Some("acme".to_owned()),
            repos: vec!["acme/api".to_owned()],
            update_types: vec![UpdateType::Minor],
            check_statuses: vec![CheckStatus::Success],
            labels: vec!["rust".to_owned()],
            dependency: Some("serde".to_owned()),
            needs_attention: true,
        };

        let expectations = [
            (
                Facet::Checks,
                PrFilter {
                    check_statuses: Vec::new(),
                    ..filter.clone()
                },
            ),
            (
                Facet::UpdateTypes,
                PrFilter {
                    update_types: Vec::new(),
                    ..filter.clone()
                },
            ),
            (
                Facet::Labels,
                PrFilter {
                    labels: Vec::new(),
                    ..filter.clone()
                },
            ),
            (
                Facet::Repositories,
                PrFilter {
                    repos: Vec::new(),
                    ..filter.clone()
                },
            ),
        ];
        for (facet, expected) in expectations {
            assert_eq!(without_facet(&filter, facet), expected, "{facet:?}");
        }
    }

    #[test]
    fn all_fields_and_a_cursor_number_their_parameters_in_clause_order() {
        let filter = PrFilter {
            query: Some("serde".to_owned()),
            owner: Some("acme".to_owned()),
            repos: vec!["acme/api".to_owned()],
            update_types: vec![UpdateType::Minor],
            check_statuses: vec![CheckStatus::Success],
            labels: vec!["rust".to_owned()],
            dependency: Some("serde".to_owned()),
            needs_attention: true,
        };

        let (sql, params) = filter_sql(&filter, Some(&cursor()), NOW).unwrap();

        assert_eq!(
            sql,
            [
                r"WHERE (LOWER(p.owner || '/' || p.repo || ' ' || p.title) LIKE ?1 ESCAPE '\' OR EXISTS (SELECT 1 FROM json_each(p.dependencies) d WHERE LOWER(json_extract(d.value, '$.name')) LIKE ?1 ESCAPE '\'))",
                "p.owner = ?2",
                "(p.owner || '/' || p.repo) IN (?3)",
                "p.update_type IN (?4)",
                "p.check_status IN (?5)",
                "EXISTS (SELECT 1 FROM json_each(p.labels) l WHERE l.value = ?6)",
                "(p.dependency = ?7 OR (p.dependency IS NULL AND EXISTS (SELECT 1 FROM json_each(p.dependencies) d WHERE json_extract(d.value, '$.name') = ?7 COLLATE NOCASE)))",
                "(p.check_status IN (?8, ?9) OR p.mergeable IN (?10) OR p.update_type = ?11 OR p.synced_at < ?12)",
                "(p.updated_at < ?13 OR (p.updated_at = ?13 AND p.id < ?14))",
            ]
            .join(" AND ")
        );
        assert_eq!(
            params,
            [
                text("%serde%"),
                text("acme"),
                text("acme/api"),
                text("minor"),
                text("success"),
                text("rust"),
                text("serde"),
                text("failure"),
                text("none"),
                text("dirty"),
                text("major"),
                Value::Integer(7_300),
                Value::Integer(500),
                text("1#7"),
            ]
        );
    }
}
