//! One owner for a dashboard read's `WHERE` clause and the parameters it
//! numbers: the filter's own, the installation scope, and whatever the
//! statement the clause goes into needs after them.

use dependaboard_core::{PageCursor, PrFilter};
use libsql::Value;

use crate::{StoreError, filter::filter_sql, integer};

/// A [`filter_sql`] clause narrowed to the pull requests of one
/// installation, owning the clause text and the parameters it numbers
/// together. Every placeholder is minted by [`bind`](ScopedFilter::bind)
/// from the vector it appends to, so a `?N` in the rendered SQL and the
/// position of its value are one allocation rather than two facts a caller
/// has to keep agreeing.
///
/// The scope predicate is a subquery over `repositories` rather than a
/// join, because the statements this clause is spliced into disagree about
/// what else is in scope: the count and the enum facet counts read
/// `pull_requests p` alone, the page joins `repositories r` for the row's
/// installation, and the repository facet holds the clause in a
/// derived table *under* a query that has already bound `r` to a different
/// `repositories` row — where an `r.installation_id = ?` would silently
/// resolve to that outer row and count the wrong thing. Naming only `p`
/// makes one predicate right in all three. `idx_repo_install` serves the
/// subquery.
pub(crate) struct ScopedFilter {
    where_sql: String,
    params: Vec<Value>,
}

impl ScopedFilter {
    /// `filter` as [`filter_sql`] renders it — continuing after `cursor`
    /// when there is one, with `now` anchoring its staleness — held to
    /// `installation_id`. Filtering and scoping are one call because the
    /// scope's parameter has to follow the filter's own, and an ordering
    /// nobody can express out of order is one nobody can get wrong.
    pub(crate) fn new(
        filter: &PrFilter,
        cursor: Option<&PageCursor>,
        now: u64,
        installation_id: u64,
    ) -> Result<Self, StoreError> {
        let (where_sql, params) = filter_sql(filter, cursor, now)?;
        let mut scoped = Self { where_sql, params };
        let installation = scoped.bind(integer(installation_id)?);
        let predicate = format!(
            "p.repository_id IN (SELECT repository_id FROM repositories WHERE installation_id = {installation})"
        );
        scoped.where_sql = if scoped.where_sql.is_empty() {
            format!("WHERE {predicate}")
        } else {
            format!("{} AND {predicate}", scoped.where_sql)
        };
        Ok(scoped)
    }

    /// Appends `value` and returns the `?N` placeholder that now stands for
    /// it. A statement that needs a parameter of its own past the filter's
    /// and the scope's gets it here, so no index is ever derived twice.
    pub(crate) fn bind(&mut self, value: Value) -> String {
        self.params.push(value);
        format!("?{}", self.params.len())
    }

    pub(crate) fn where_sql(&self) -> &str {
        &self.where_sql
    }

    /// The parameters in the order their placeholders number them, to bind
    /// alongside the statement the clause went into.
    pub(crate) fn into_params(self) -> Vec<Value> {
        self.params
    }
}
