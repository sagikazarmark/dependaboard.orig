//! GitHub's GraphQL API: the pull-request snapshot query, the wire shapes it answers with,
//! and how those map onto the same domain vocabulary the REST client used.
//!
//! GraphQL spells its enums in upper snake case (`STARTUP_FAILURE`, `IN_PROGRESS`) where
//! REST spells them lower (`startup_failure`, `in_progress`), and the two vocabularies are
//! otherwise identical, so every value is lowercased and handed to the core mappings
//! (`check_signal`, `status_signal`, `Mergeable::from_github_state`) rather than mapped
//! twice.

use dependaboard_core::{
    CheckSignal, DEPENDABOT_LOGIN, GithubErrorResponse, Mergeable, PrKey, PrRecord, SyncRequest,
    check_signal, highest_update_type, parse_dependabot_metadata, rollup_checks, status_signal,
};
use serde::Deserialize;

use crate::{
    GithubError,
    rest::{RateLimitHeaders, parse_timestamp},
};

/// What a snapshot reads off a commit: its message, and the first page each of its check
/// contexts (check runs and commit statuses together, under `statusCheckRollup`) and its
/// check suites. Suites are read separately because a suite that failed at startup has no
/// runs to show up as contexts. Both are paged at 100, the same page size the REST reads
/// used, and the rollup is computed by the client rather than taken from
/// `statusCheckRollup.state` so its precedence stays ours and suite-level failures count.
const COMMIT_SELECTION: &str = r#"
        oid
        message
        statusCheckRollup {
          contexts(first: 100) {
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ... on CheckRun { status conclusion }
              ... on StatusContext { state }
            }
          }
        }
        checkSuites(first: 100) {
          pageInfo { hasNextPage endCursor }
          nodes { status conclusion }
        }
"#;

/// Everything one pull-request snapshot needs, in one request.
///
/// The head commit is taken from `commits(last: 1)`. GitHub orders that list by date, so
/// a commit pushed on top with an older date is not listed last; the client compares its
/// `oid` with `headRefOid` and reads the head with [`head_commit_query`] when they differ.
pub(crate) fn snapshot_query() -> String {
    format!(
        r#"
query PullRequestSnapshot($owner: String!, $repo: String!, $number: Int!) {{
  rateLimit {{ cost remaining }}
  repository(owner: $owner, name: $repo) {{
    pullRequest(number: $number) {{
      title
      url
      state
      author {{ __typename login }}
      createdAt
      updatedAt
      mergeStateStatus
      headRefOid
      labels(first: 100) {{ nodes {{ name }} }}
      commits(last: 1) {{
        nodes {{
          commit {{{COMMIT_SELECTION}
          }}
        }}
      }}
    }}
  }}
}}
"#
    )
}

/// The head commit by SHA, for a pull request whose last listed commit is not its head.
pub(crate) fn head_commit_query() -> String {
    format!(
        r#"
query HeadCommit($owner: String!, $repo: String!, $sha: GitObjectID!) {{
  repository(owner: $owner, name: $repo) {{
    object(oid: $sha) {{
      ... on Commit {{{COMMIT_SELECTION}
      }}
    }}
  }}
}}
"#
    )
}

/// The next page of a head commit's check contexts, for the rare commit with more than a
/// hundred check runs and statuses. `$sha` addresses the commit directly so the page does
/// not depend on the pull request's head staying put between requests.
pub(crate) const CHECK_CONTEXTS_PAGE_QUERY: &str = r#"
query CheckContextsPage($owner: String!, $repo: String!, $sha: GitObjectID!, $after: String!) {
  repository(owner: $owner, name: $repo) {
    object(oid: $sha) {
      ... on Commit {
        statusCheckRollup {
          contexts(first: 100, after: $after) {
            pageInfo { hasNextPage endCursor }
            nodes {
              __typename
              ... on CheckRun { status conclusion }
              ... on StatusContext { state }
            }
          }
        }
      }
    }
  }
}
"#;

/// The next page of a head commit's check suites; see [`CHECK_CONTEXTS_PAGE_QUERY`].
pub(crate) const CHECK_SUITES_PAGE_QUERY: &str = r#"
query CheckSuitesPage($owner: String!, $repo: String!, $sha: GitObjectID!, $after: String!) {
  repository(owner: $owner, name: $repo) {
    object(oid: $sha) {
      ... on Commit {
        checkSuites(first: 100, after: $after) {
          pageInfo { hasNextPage endCursor }
          nodes { status conclusion }
        }
      }
    }
  }
}
"#;

/// The body of a GraphQL answer. GitHub reports most failures here, inside a `200`, so
/// `errors` must be read before `data` is trusted.
#[derive(Debug, Deserialize)]
pub(crate) struct GraphqlResponse<T> {
    pub(crate) data: Option<T>,
    #[serde(default)]
    pub(crate) errors: Vec<GraphqlError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GraphqlError {
    /// GitHub's error class (`NOT_FOUND`, `RATE_LIMITED`, ...); absent on execution
    /// failures such as query timeouts.
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) message: String,
}

/// The HTTP status the REST API would have answered with for a GraphQL error class.
///
/// GraphQL failures are folded into [`GithubErrorResponse`] so that the caller's
/// classification does not care which API answered: `NOT_FOUND` on a pull request the
/// caller has already seen means "gone" exactly as a REST 404 does. Errors GitHub leaves
/// untyped are execution failures ("Something went wrong while executing your query")
/// and are worth a retry, so they read as a bad gateway; other typed errors are query
/// problems no retry will fix.
pub(crate) fn error_status(kind: Option<&str>) -> u16 {
    match kind {
        Some("NOT_FOUND") => 404,
        Some("FORBIDDEN" | "INSUFFICIENT_SCOPES") => 403,
        Some("RATE_LIMITED") => 429,
        Some("SERVICE_UNAVAILABLE") => 503,
        Some(_) => 422,
        None => 502,
    }
}

/// Folds the errors of one answer into the response shape the rest of the system
/// classifies. The first classified error decides the status, so an execution hiccup
/// listed alongside a `NOT_FOUND` does not hide it; every message is kept.
pub(crate) fn error_response(
    errors: &[GraphqlError],
    rate_limit: RateLimitHeaders,
) -> GithubErrorResponse {
    let kind = errors.iter().find_map(|error| error.kind.as_deref());
    let message = errors
        .iter()
        .map(|error| error.message.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    rate_limit.error(error_status(kind), message, None)
}

/// One page of a GraphQL connection.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Connection<T> {
    #[serde(default)]
    pub(crate) page_info: PageInfo,
    // Spelled out so serde does not demand `T: Default` for the empty case.
    #[serde(default = "Vec::new")]
    pub(crate) nodes: Vec<T>,
}

impl<T> Connection<T> {
    /// The cursor to continue from, when there is another page.
    pub(crate) fn next_cursor(&self) -> Option<&str> {
        if self.page_info.has_next_page {
            self.page_info.end_cursor.as_deref()
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageInfo {
    #[serde(default)]
    pub(crate) has_next_page: bool,
    pub(crate) end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotData {
    pub(crate) rate_limit: Option<RateLimit>,
    pub(crate) repository: Option<SnapshotRepository>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RateLimit {
    pub(crate) cost: u64,
    pub(crate) remaining: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotRepository {
    pub(crate) pull_request: Option<PullRequestNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PullRequestNode {
    pub(crate) title: String,
    pub(crate) url: String,
    /// `OPEN`, `CLOSED`, or `MERGED`.
    pub(crate) state: String,
    /// Absent when the authoring account no longer exists.
    pub(crate) author: Option<Actor>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    /// Non-null in the schema; tolerated absent, as REST's `mergeable_state` could be,
    /// and read as unknown.
    pub(crate) merge_state_status: Option<String>,
    pub(crate) head_ref_oid: String,
    pub(crate) labels: Option<Connection<Label>>,
    pub(crate) commits: Connection<PullRequestCommit>,
}

impl PullRequestNode {
    /// Whether this is an open pull request authored by Dependabot, the only kind the
    /// dashboard projects.
    pub(crate) fn is_open_dependabot_pull(&self) -> bool {
        self.state == "OPEN"
            && self
                .author
                .as_ref()
                .is_some_and(|author| author.rest_login() == DEPENDABOT_LOGIN)
    }

    /// The pull request's mergeability in REST's spelling: `mergeStateStatus` lowercased is
    /// `mergeable_state` value for value.
    pub(crate) fn mergeable_state(&self) -> Option<String> {
        lowercase(self.merge_state_status.as_deref())
    }

    /// The last commit GitHub lists for the pull request, which is its head unless the
    /// listing's date order says otherwise; see [`snapshot_query`].
    pub(crate) fn last_listed_commit(&self) -> Option<&Commit> {
        self.commits.nodes.first().map(|listed| &listed.commit)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Actor {
    #[serde(rename = "__typename")]
    pub(crate) typename: String,
    pub(crate) login: String,
}

impl Actor {
    /// The login as REST reports it. REST marks GitHub Apps with a `[bot]` suffix that
    /// GraphQL leaves off (`dependabot[bot]` versus a `Bot` whose login is `dependabot`);
    /// adding it back lets the same [`DEPENDABOT_LOGIN`] comparison serve both APIs.
    pub(crate) fn rest_login(&self) -> String {
        if self.typename == "Bot" {
            format!("{}[bot]", self.login)
        } else {
            self.login.clone()
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct Label {
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PullRequestCommit {
    pub(crate) commit: Commit,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Commit {
    pub(crate) oid: String,
    pub(crate) message: String,
    /// `null` when the commit has neither check runs nor commit statuses.
    pub(crate) status_check_rollup: Option<StatusCheckRollup>,
    pub(crate) check_suites: Option<Connection<CheckSuite>>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct StatusCheckRollup {
    pub(crate) contexts: Connection<CheckContext>,
}

/// The answer to a query addressing a commit by SHA (`repository.object`): the whole
/// commit for [`head_commit_query`], or just the paged connection for a page query.
#[derive(Debug, Deserialize)]
pub(crate) struct CommitData<T> {
    repository: Option<CommitRepository<T>>,
}

#[derive(Debug, Deserialize)]
struct CommitRepository<T> {
    object: Option<T>,
}

impl<T> CommitData<T> {
    pub(crate) fn into_commit(self) -> Option<T> {
        self.repository.and_then(|repository| repository.object)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckContextsPage {
    pub(crate) status_check_rollup: Option<StatusCheckRollup>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckSuitesPage {
    pub(crate) check_suites: Option<Connection<CheckSuite>>,
}

/// A member of `statusCheckRollup.contexts`: a check run or a legacy commit status.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "__typename")]
pub(crate) enum CheckContext {
    CheckRun {
        status: Option<String>,
        conclusion: Option<String>,
    },
    StatusContext {
        state: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct CheckSuite {
    pub(crate) status: Option<String>,
    pub(crate) conclusion: Option<String>,
}

/// What one rollup context contributes to the check rollup.
///
/// REST's combined status needed its `total_count` to tell "no statuses" from "pending";
/// here each status context maps on its own state, and a commit with none has nothing to
/// map, which is the same distinction without the counter.
pub(crate) fn check_context_signal(context: &CheckContext) -> Option<CheckSignal> {
    match context {
        CheckContext::CheckRun { status, conclusion } => check_signal(
            lowercase(status.as_deref()).as_deref(),
            lowercase(conclusion.as_deref()).as_deref(),
        ),
        CheckContext::StatusContext { state } => status_signal(&state.to_ascii_lowercase()),
    }
}

/// What one check suite contributes: only failures. Runs carry active and pass state;
/// suites add the failures that can happen before any run exists, such as a workflow
/// that failed to start.
pub(crate) fn check_suite_signal(suite: &CheckSuite) -> Option<CheckSignal> {
    check_signal(
        lowercase(suite.status.as_deref()).as_deref(),
        lowercase(suite.conclusion.as_deref()).as_deref(),
    )
    .filter(|signal| matches!(signal, CheckSignal::Fail))
}

/// A GraphQL enum value in REST's spelling.
fn lowercase(value: Option<&str>) -> Option<String> {
    value.map(str::to_ascii_lowercase)
}

/// The record the dashboard keeps of `request`'s pull request, projected from what
/// GitHub answered: `pull` as the snapshot query read it, `head` the commit at its
/// `headRefOid`, and `signals` everything the head's check runs, statuses and suites
/// contributed across every page. The caller has already found `pull` to be an open
/// Dependabot pull request (see [`PullRequestNode::is_open_dependabot_pull`]); a closed
/// one is never projected, and is not read this far.
///
/// The Dependabot metadata is parsed from the head commit's message with the title as
/// the fallback; a grouped update leaves the scalar dependency columns empty and keeps
/// every dependency in `dependencies`. Fails only on a timestamp that cannot be read.
pub(crate) fn project_snapshot(
    request: &SyncRequest,
    installation_id: u64,
    pull: &PullRequestNode,
    head: &Commit,
    signals: impl IntoIterator<Item = CheckSignal>,
    synced_at: u64,
) -> Result<PrRecord, GithubError> {
    let mergeable = pull.mergeable_state().map_or(Mergeable::Unknown, |state| {
        Mergeable::from_github_state(&state)
    });
    let labels = pull
        .labels
        .as_ref()
        .map(|labels| {
            labels
                .nodes
                .iter()
                .map(|label| label.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let dependencies = parse_dependabot_metadata(&head.message, &pull.title);
    let (dependency, from_version, to_version) = if dependencies.len() == 1 {
        let dependency = &dependencies[0];
        (
            Some(dependency.name.clone()),
            dependency.from_version.clone(),
            dependency.to_version.clone(),
        )
    } else {
        (None, None, None)
    };
    Ok(PrRecord {
        id: PrKey::new(request.repository_id, request.number).to_string(),
        repository_id: request.repository_id,
        installation_id,
        owner: request.owner.clone(),
        repo: request.repo.clone(),
        number: request.number,
        title: pull.title.clone(),
        html_url: pull.url.clone(),
        dependency,
        from_version,
        to_version,
        update_type: highest_update_type(&dependencies),
        dependencies,
        head_sha: head.oid.clone(),
        check_status: rollup_checks(signals),
        mergeable,
        labels,
        created_at: parse_timestamp(&pull.created_at)?,
        updated_at: parse_timestamp(&pull.updated_at)?,
        synced_at,
    })
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{
        CheckStatus, DependencyUpdate, Mergeable, PrRecord, SyncRequest, UpdateType, rollup_checks,
    };
    use serde_json::{Value, json};

    use super::*;

    fn run(status: &str, conclusion: Option<&str>) -> CheckContext {
        CheckContext::CheckRun {
            status: Some(status.to_owned()),
            conclusion: conclusion.map(str::to_owned),
        }
    }

    fn status(state: &str) -> CheckContext {
        CheckContext::StatusContext {
            state: state.to_owned(),
        }
    }

    fn suite(status: &str, conclusion: Option<&str>) -> CheckSuite {
        CheckSuite {
            status: Some(status.to_owned()),
            conclusion: conclusion.map(str::to_owned),
        }
    }

    #[test]
    fn graphql_check_vocabulary_maps_onto_the_rest_truth_table() {
        for conclusion in ["SUCCESS", "NEUTRAL", "SKIPPED"] {
            assert_eq!(
                check_context_signal(&run("COMPLETED", Some(conclusion))),
                Some(CheckSignal::Pass),
                "{conclusion}"
            );
        }
        for conclusion in [
            "FAILURE",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "CANCELLED",
            "STALE",
            "STARTUP_FAILURE",
        ] {
            assert_eq!(
                check_context_signal(&run("COMPLETED", Some(conclusion))),
                Some(CheckSignal::Fail),
                "{conclusion}"
            );
        }
        for status in ["QUEUED", "IN_PROGRESS", "WAITING", "PENDING", "REQUESTED"] {
            assert_eq!(
                check_context_signal(&run(status, None)),
                Some(CheckSignal::Pending),
                "{status}"
            );
        }
        assert_eq!(check_context_signal(&run("COMPLETED", None)), None);

        assert_eq!(
            check_context_signal(&status("SUCCESS")),
            Some(CheckSignal::Pass)
        );
        for state in ["FAILURE", "ERROR"] {
            assert_eq!(
                check_context_signal(&status(state)),
                Some(CheckSignal::Fail),
                "{state}"
            );
        }
        for state in ["PENDING", "EXPECTED"] {
            assert_eq!(
                check_context_signal(&status(state)),
                Some(CheckSignal::Pending),
                "{state}"
            );
        }
    }

    #[test]
    fn suites_only_contribute_failures() {
        assert_eq!(
            check_suite_signal(&suite("COMPLETED", Some("STARTUP_FAILURE"))),
            Some(CheckSignal::Fail)
        );
        assert_eq!(
            check_suite_signal(&suite("COMPLETED", Some("FAILURE"))),
            Some(CheckSignal::Fail)
        );
        // A suite still queued next to a finished run must not drag the rollup back to pending,
        // and a passing suite says nothing its runs have not already said.
        assert_eq!(check_suite_signal(&suite("QUEUED", None)), None);
        assert_eq!(
            check_suite_signal(&suite("COMPLETED", Some("SUCCESS"))),
            None
        );
    }

    #[test]
    fn rollup_precedence_is_fail_then_pending_then_pass_then_none() {
        let signals = |contexts: &[CheckContext], suites: &[CheckSuite]| {
            rollup_checks(
                contexts
                    .iter()
                    .filter_map(check_context_signal)
                    .chain(suites.iter().filter_map(check_suite_signal)),
            )
        };

        assert_eq!(
            signals(
                &[
                    run("IN_PROGRESS", None),
                    run("COMPLETED", Some("SUCCESS")),
                    status("FAILURE")
                ],
                &[]
            ),
            CheckStatus::Failure,
            "a failure beats a still-running check"
        );
        assert_eq!(
            signals(
                &[run("COMPLETED", Some("SUCCESS"))],
                &[suite("COMPLETED", Some("STARTUP_FAILURE"))]
            ),
            CheckStatus::Failure,
            "a workflow that failed to start fails the rollup even with no run to show for it"
        );
        assert_eq!(
            signals(&[run("COMPLETED", Some("SUCCESS")), status("PENDING")], &[]),
            CheckStatus::Pending,
            "a pending status beats a passing run"
        );
        assert_eq!(
            signals(
                &[run("COMPLETED", Some("SKIPPED")), status("SUCCESS")],
                &[suite("QUEUED", None)]
            ),
            CheckStatus::Success,
            "a queued suite container does not override finished checks"
        );
        assert_eq!(signals(&[], &[]), CheckStatus::None);
        assert_eq!(
            signals(&[], &[suite("COMPLETED", Some("SUCCESS"))]),
            CheckStatus::None,
            "a passing suite with nothing in it reports nothing"
        );
    }

    fn graphql_error(kind: Option<&str>, message: &str) -> GraphqlError {
        GraphqlError {
            kind: kind.map(str::to_owned),
            message: message.to_owned(),
        }
    }

    /// The statuses are chosen for what `classify_github_error` makes of them: 404 is the
    /// "gone or never visible" decision, 429 is a rate limit, 5xx retries, 4xx is final.
    #[test]
    fn graphql_error_classes_read_as_the_rest_statuses_they_stand_for() {
        assert_eq!(error_status(Some("NOT_FOUND")), 404);
        assert_eq!(error_status(Some("FORBIDDEN")), 403);
        assert_eq!(error_status(Some("INSUFFICIENT_SCOPES")), 403);
        assert_eq!(error_status(Some("RATE_LIMITED")), 429);
        assert_eq!(error_status(Some("SERVICE_UNAVAILABLE")), 503);
        assert_eq!(error_status(Some("MAX_NODE_LIMIT_EXCEEDED")), 422);
        assert_eq!(
            error_status(None),
            502,
            "an untyped error is a query execution failure worth retrying"
        );
    }

    #[test]
    fn an_error_response_keeps_every_message_and_the_headers() {
        // The execution hiccup listed first must not hide the classified error behind it.
        let response = error_response(
            &[
                graphql_error(None, "Something went wrong while executing your query."),
                graphql_error(Some("NOT_FOUND"), "Could not resolve to a Repository."),
            ],
            RateLimitHeaders {
                remaining: Some(4_990),
                reset: Some(1_700_000_000),
                retry_after: None,
            },
        );

        assert_eq!(
            response,
            GithubErrorResponse {
                status: 404,
                message: "Something went wrong while executing your query.; Could not resolve to a Repository.".to_owned(),
                documentation_url: None,
                rate_limit_remaining: Some(4_990),
                rate_limit_reset: Some(1_700_000_000),
                retry_after_seconds: None,
            }
        );
    }

    // --- the snapshot projection ------------------------------------------

    const INSTALLATION_ID: u64 = 42;
    const REPOSITORY_ID: u64 = 7;
    const OWNER: &str = "acme";
    const REPO: &str = "api";
    const NUMBER: u64 = 9;
    const HEAD_SHA: &str = "abc123";
    const CREATED_AT: &str = "2024-05-01T10:00:00Z";
    const UPDATED_AT: &str = "2024-05-02T11:30:00Z";
    const SYNCED_AT: u64 = 1_700_000_000;

    fn sync_request() -> SyncRequest {
        SyncRequest {
            repository_id: REPOSITORY_ID,
            owner: OWNER.to_owned(),
            repo: REPO.to_owned(),
            number: NUMBER,
            bypass_debounce: false,
            completion_id: None,
        }
    }

    fn unix(rfc3339: &str) -> u64 {
        u64::try_from(
            chrono::DateTime::parse_from_rfc3339(rfc3339)
                .unwrap()
                .timestamp(),
        )
        .unwrap()
    }

    /// The head commit as GraphQL reports it, at [`HEAD_SHA`] with `message`; its check
    /// pages are the client's to read, so the projection never looks at them.
    fn head_commit(message: &str) -> Commit {
        serde_json::from_value(json!({
            "oid": HEAD_SHA,
            "message": message,
            "statusCheckRollup": null,
            "checkSuites": null,
        }))
        .expect("a commit as the snapshot query answers with it")
    }

    /// GraphQL's view of an open pull request authored by the Dependabot app (which
    /// GraphQL reports as the `Bot` named `dependabot`, where REST says
    /// `dependabot[bot]`), at [`HEAD_SHA`].
    fn pull_request_node() -> Value {
        json!({
            "title": "Bump serde from 1.0.1 to 1.0.2",
            "url": format!("https://github.com/{OWNER}/{REPO}/pull/{NUMBER}"),
            "state": "OPEN",
            "author": { "__typename": "Bot", "login": "dependabot" },
            "createdAt": CREATED_AT,
            "updatedAt": UPDATED_AT,
            "mergeStateStatus": "CLEAN",
            "headRefOid": HEAD_SHA,
            "labels": { "nodes": [{ "name": "dependencies" }, { "name": "rust" }] },
            "commits": { "nodes": [{ "commit": { "oid": HEAD_SHA, "message": "Bump serde from 1.0.1 to 1.0.2" } }] },
        })
    }

    fn pull(node: Value) -> PullRequestNode {
        serde_json::from_value(node).expect("a pull request as the snapshot query answers with it")
    }

    /// The expected record is the one the REST-backed client produced for this pull
    /// request before snapshots moved to GraphQL; the pull request as GraphQL describes
    /// it, with what its checks signalled, must project to it unchanged.
    #[test]
    fn a_single_dependency_pull_request_projects_to_its_record() {
        let pull = pull(pull_request_node());
        let head = head_commit(
            "Bump serde from 1.0.1 to 1.0.2\n\n---\nupdated-dependencies:\n- dependency-name: serde\n  dependency-type: direct:production\n  update-type: version-update:semver-patch\n...",
        );

        let record = project_snapshot(
            &sync_request(),
            INSTALLATION_ID,
            &pull,
            &head,
            [CheckSignal::Pass, CheckSignal::Pass],
            SYNCED_AT,
        )
        .unwrap();

        assert_eq!(
            record,
            PrRecord {
                id: "7#9".to_owned(),
                repository_id: REPOSITORY_ID,
                installation_id: INSTALLATION_ID,
                owner: OWNER.to_owned(),
                repo: REPO.to_owned(),
                number: NUMBER,
                title: "Bump serde from 1.0.1 to 1.0.2".to_owned(),
                html_url: format!("https://github.com/{OWNER}/{REPO}/pull/{NUMBER}"),
                dependency: Some("serde".to_owned()),
                from_version: Some("1.0.1".to_owned()),
                to_version: Some("1.0.2".to_owned()),
                dependencies: vec![DependencyUpdate {
                    name: "serde".to_owned(),
                    from_version: Some("1.0.1".to_owned()),
                    to_version: Some("1.0.2".to_owned()),
                    update_type: UpdateType::Patch,
                }],
                update_type: UpdateType::Patch,
                head_sha: HEAD_SHA.to_owned(),
                check_status: CheckStatus::Success,
                mergeable: Mergeable::Clean,
                labels: vec!["dependencies".to_owned(), "rust".to_owned()],
                created_at: unix(CREATED_AT),
                updated_at: unix(UPDATED_AT),
                synced_at: SYNCED_AT,
            }
        );
    }

    #[test]
    fn a_grouped_pull_request_keeps_every_dependency_and_leaves_the_scalar_columns_empty() {
        let mut node = pull_request_node();
        node["title"] = json!("Bump the cargo group with 2 updates");
        node["mergeStateStatus"] = json!("BEHIND");
        let pull = pull(node);
        let head = head_commit(
            "Bump the cargo group with 2 updates\n\n---\nupdated-dependencies:\n- dependency-name: tokio\n  update-type: version-update:semver-minor\n- dependency-name: serde\n  update-type: version-update:semver-major\n...",
        );

        // The head's one signal is a suite that failed to start, with no run to show for it.
        let record = project_snapshot(
            &sync_request(),
            INSTALLATION_ID,
            &pull,
            &head,
            [CheckSignal::Fail],
            SYNCED_AT,
        )
        .unwrap();

        assert_eq!(record.dependency, None);
        assert_eq!(record.from_version, None);
        assert_eq!(record.to_version, None);
        assert_eq!(
            record
                .dependencies
                .iter()
                .map(|dependency| dependency.name.as_str())
                .collect::<Vec<_>>(),
            ["tokio", "serde"]
        );
        assert_eq!(record.update_type, UpdateType::Major);
        assert_eq!(record.check_status, CheckStatus::Failure);
        assert_eq!(record.mergeable, Mergeable::Behind);
    }

    /// Only an open pull request of the Dependabot app is one the dashboard projects. A
    /// user account named `dependabot` is not the app: only GraphQL's `Bot` maps onto
    /// REST's `dependabot[bot]`. A pull request whose author no longer exists has none.
    #[test]
    fn only_an_open_pull_request_of_the_dependabot_app_is_one_to_project() {
        assert!(pull(pull_request_node()).is_open_dependabot_pull());

        for author in [
            json!({ "__typename": "User", "login": "octocat" }),
            json!({ "__typename": "User", "login": "dependabot" }),
            Value::Null,
        ] {
            let mut node = pull_request_node();
            node["author"] = author.clone();
            assert!(!pull(node).is_open_dependabot_pull(), "author {author}");
        }
        for state in ["CLOSED", "MERGED"] {
            let mut node = pull_request_node();
            node["state"] = json!(state);
            assert!(!pull(node).is_open_dependabot_pull(), "state {state}");
        }
    }
}
