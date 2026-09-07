//! HTTP-level tests for `GithubClient` against a mock GitHub API.
//!
//! Every test drives the public `GithubApi` surface through a `wiremock`
//! server so that the client's wire behaviour (token handling, pagination,
//! the mutate-verify-confirm paths and error parsing) is pinned independently
//! of how the client is structured internally.

use dependaboard_core::{
    CheckStatus, CommandRequest, DEPENDABOT_LOGIN, DependabotCommand, DependencyUpdate,
    GithubErrorResponse, MergeMethod, MergeRequest, Mergeable, Operation, PrRecord, PrTarget,
    SyncRequest, UpdateBranchRequest, UpdateType, UserId, unix_seconds,
};
use dependaboard_github::{
    GithubApi, GithubClient, GithubConfig, GithubError, Merged, ProtocolError,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockBuilder, MockServer, ResponseTemplate,
    matchers::{
        bearer_token, body_json, body_partial_json, body_string_contains, header_regex, method,
        path, query_param,
    },
};

/// A throwaway RSA key generated for these tests; it has never signed
/// anything outside this test suite.
const APP_KEY: &str = include_str!("../testdata/app-key.pem");
const APP_ID: u64 = 1234;
const INSTALLATION_ID: u64 = 42;
const USER_PAT: &str = "ghp_dashboard_user_pat";
const DASHBOARD_USER: &str = "dependaboard";
const TOKEN: &str = "ghs_installation_token";
const FAR_FUTURE: &str = "2099-01-01T00:00:00Z";
const OWNER: &str = "acme";
const REPO: &str = "api";
const REPOSITORY_ID: u64 = 7;

fn config(server: &MockServer) -> GithubConfig {
    GithubConfig {
        api_url: server.uri(),
        app_id: APP_ID,
        installation_id: INSTALLATION_ID,
        private_key: SecretString::from(APP_KEY),
        user_pat: Some(SecretString::from(USER_PAT)),
        dashboard_user: UserId::new(DASHBOARD_USER),
        merge_method: MergeMethod::Squash,
    }
}

fn client(server: &MockServer) -> GithubClient {
    GithubClient::new(config(server)).expect("test config builds a client")
}

/// The App-authenticated token exchange: a `POST` signed with an RS256 JWT.
fn token_exchange() -> MockBuilder {
    Mock::given(method("POST"))
        .and(path(format!(
            "/app/installations/{INSTALLATION_ID}/access_tokens"
        )))
        .and(header_regex(
            "authorization",
            r"^Bearer [\w-]+\.[\w-]+\.[\w-]+$",
        ))
}

fn token_response(token: &str, expires_at: &str) -> ResponseTemplate {
    ResponseTemplate::new(201).set_body_json(json!({ "token": token, "expires_at": expires_at }))
}

/// Mounts a token exchange that always answers with [`TOKEN`].
async fn mount_token(server: &MockServer) {
    token_exchange()
        .respond_with(token_response(TOKEN, FAR_FUTURE))
        .mount(server)
        .await;
}

/// The repository listing, matched on the installation token it was sent with.
fn repositories_endpoint(token: &str) -> MockBuilder {
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(bearer_token(token))
}

fn no_repositories() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "total_count": 0, "repositories": [] }))
}

fn rfc3339_in(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339()
}

fn github_error(status: u16, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "message": message }))
}

fn http_response(error: &GithubError) -> Option<&GithubErrorResponse> {
    match error {
        GithubError::Http { response, .. } => Some(response),
        _ => None,
    }
}

// --- construction ---------------------------------------------------------

#[tokio::test]
async fn invalid_private_key_fails_client_construction() {
    let server = MockServer::start().await;
    let config = GithubConfig {
        private_key: SecretString::from("-----BEGIN RSA PRIVATE KEY-----\nnot a key\n"),
        ..config(&server)
    };

    let error = GithubClient::new(config).err().expect("construction fails");

    assert!(
        matches!(&error, GithubError::Config(message) if message.contains("private key")),
        "expected a private-key config error, got {error:?}"
    );
}

// --- installation tokens --------------------------------------------------

#[tokio::test]
async fn installation_token_is_cached_across_requests() {
    let server = MockServer::start().await;
    token_exchange()
        .respond_with(token_response(TOKEN, FAR_FUTURE))
        .expect(1)
        .mount(&server)
        .await;
    repositories_endpoint(TOKEN)
        .respond_with(no_repositories())
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server);

    client.list_installation_repositories().await.unwrap();
    client.list_installation_repositories().await.unwrap();

    server.verify().await;
}

#[tokio::test]
async fn installation_token_is_refetched_when_close_to_expiry() {
    let server = MockServer::start().await;
    // Inside the refresh margin: valid, but not worth reusing.
    token_exchange()
        .respond_with(token_response("ghs_expiring", &rfc3339_in(30)))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    token_exchange()
        .respond_with(token_response("ghs_fresh", FAR_FUTURE))
        .expect(1)
        .mount(&server)
        .await;
    for token in ["ghs_expiring", "ghs_fresh"] {
        repositories_endpoint(token)
            .respond_with(no_repositories())
            .expect(1)
            .mount(&server)
            .await;
    }
    let client = client(&server);

    client.list_installation_repositories().await.unwrap();
    client.list_installation_repositories().await.unwrap();

    server.verify().await;
}

#[tokio::test]
async fn unauthorized_response_refreshes_the_token_once_and_keeps_the_new_one() {
    let server = MockServer::start().await;
    token_exchange()
        .respond_with(token_response("ghs_revoked", FAR_FUTURE))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    token_exchange()
        .respond_with(token_response("ghs_fresh", FAR_FUTURE))
        .expect(1)
        .mount(&server)
        .await;
    repositories_endpoint("ghs_revoked")
        .respond_with(github_error(401, "Bad credentials"))
        .expect(1)
        .mount(&server)
        .await;
    // Once for the retry, once more for the follow-up call: the refreshed
    // token must have replaced the revoked one in the cache.
    repositories_endpoint("ghs_fresh")
        .respond_with(no_repositories())
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server);

    let repositories = client.list_installation_repositories().await;
    assert!(repositories.is_ok(), "{repositories:?}");
    client.list_installation_repositories().await.unwrap();

    server.verify().await;
}

#[tokio::test]
async fn persistent_unauthorized_response_is_returned_after_one_retry() {
    let server = MockServer::start().await;
    token_exchange()
        .respond_with(token_response(TOKEN, FAR_FUTURE))
        .expect(2)
        .mount(&server)
        .await;
    repositories_endpoint(TOKEN)
        .respond_with(github_error(401, "Bad credentials"))
        .expect(2)
        .mount(&server)
        .await;

    let error = client(&server)
        .list_installation_repositories()
        .await
        .expect_err("401 surfaces after the retry");

    let response = http_response(&error).expect("an HTTP error");
    assert_eq!(response.status, 401);
    assert_eq!(response.message, "Bad credentials");
    server.verify().await;
}

// --- pagination -----------------------------------------------------------

fn repository(id: u64) -> Value {
    json!({ "id": id, "name": format!("repo-{id}"), "owner": { "login": OWNER } })
}

/// One page of `GET /installation/repositories`, as GitHub shapes it: the page's
/// repositories beside the installation's total.
fn repositories_page(total_count: usize, repositories: Vec<Value>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "total_count": total_count,
        "repositories": repositories,
    }))
}

/// Mounts page `page` of the repository listing, expected to be fetched exactly once.
async fn mount_repositories_page(
    server: &MockServer,
    page: usize,
    total_count: usize,
    ids: impl IntoIterator<Item = u64>,
) {
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param("per_page", "100"))
        .and(query_param("page", page.to_string()))
        .respond_with(repositories_page(
            total_count,
            ids.into_iter().map(repository).collect(),
        ))
        .expect(1)
        .mount(server)
        .await;
}

fn pull_list_item(number: u64, login: &str) -> Value {
    json!({ "number": number, "user": { "login": login } })
}

#[tokio::test]
async fn repository_listing_stops_after_the_first_short_page() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_repositories_page(&server, 1, 103, 1..=100).await;
    mount_repositories_page(&server, 2, 103, 101..=103).await;

    let repositories = client(&server)
        .list_installation_repositories()
        .await
        .unwrap();

    assert_eq!(repositories.len(), 103);
    assert_eq!(repositories[0].repository_id, 1);
    assert_eq!(repositories[0].installation_id, INSTALLATION_ID);
    assert_eq!(repositories[0].owner, OWNER);
    assert_eq!(repositories[0].repo, "repo-1");
    assert_eq!(repositories[102].repository_id, 103);
    assert_eq!(
        request_count(&server, "GET", "/installation/repositories").await,
        2,
        "no request is made for a third page"
    );
    server.verify().await;
}

#[tokio::test]
async fn repository_listing_fails_retryably_when_a_repository_leaves_between_pages() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    // Repository 50 is removed from the installation after the first page is served, so
    // the second page starts one position early: 101 is never listed, and the total
    // GitHub reports has moved. Offset pagination cannot say which one went.
    mount_repositories_page(&server, 1, 150, 1..=100).await;
    mount_repositories_page(&server, 2, 149, 102..=150).await;

    let error = client(&server)
        .list_installation_repositories()
        .await
        .unwrap_err();

    assert!(
        matches!(error, GithubError::Shifted { .. }),
        "a listing that moved under its pages is not an authoritative set: {error:?}"
    );
    server.verify().await;
}

#[tokio::test]
async fn repository_listing_fails_retryably_when_the_pages_do_not_add_up_to_the_total() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    // One repository leaves the tail and another joins the head between the pages: the
    // total stands, but the page boundary shifted and repository 100 is listed twice while
    // the newcomer, sorted before it, is never seen.
    mount_repositories_page(&server, 1, 101, 1..=100).await;
    mount_repositories_page(&server, 2, 101, [100]).await;

    let error = client(&server)
        .list_installation_repositories()
        .await
        .unwrap_err();

    assert!(
        matches!(error, GithubError::Shifted { .. }),
        "a duplicate at the boundary means something else was dropped: {error:?}"
    );
    server.verify().await;
}

#[tokio::test]
async fn repository_listing_stops_once_the_total_is_reached() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_repositories_page(&server, 1, 200, 1..=100).await;
    mount_repositories_page(&server, 2, 200, 101..=200).await;

    let repositories = client(&server)
        .list_installation_repositories()
        .await
        .unwrap();

    assert_eq!(repositories.len(), 200);
    assert_eq!(
        request_count(&server, "GET", "/installation/repositories").await,
        2,
        "the total says there is no third page to ask for"
    );
    server.verify().await;
}

#[tokio::test]
async fn dependabot_pull_listing_paginates_and_drops_other_authors() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let first_page: Vec<Value> = (1..=100)
        .map(|number| {
            let login = if number % 2 == 0 {
                DEPENDABOT_LOGIN
            } else {
                "octocat"
            };
            pull_list_item(number, login)
        })
        .collect();
    let second_page = vec![pull_list_item(101, DEPENDABOT_LOGIN)];
    for (index, page) in [first_page, second_page].into_iter().enumerate() {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
            .and(query_param("state", "open"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", (index + 1).to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .expect(1)
            .mount(&server)
            .await;
    }

    let requests = client(&server)
        .list_dependabot_prs(OWNER, REPO, REPOSITORY_ID)
        .await
        .unwrap();

    assert_eq!(requests.len(), 51);
    assert!(
        requests
            .iter()
            .all(|request| request.number % 2 == 0 || request.number == 101)
    );
    assert_eq!(
        requests[0],
        SyncRequest {
            repository_id: REPOSITORY_ID,
            owner: OWNER.to_owned(),
            repo: REPO.to_owned(),
            number: 2,
            bypass_debounce: false,
            completion_id: None,
        }
    );
    server.verify().await;
}

#[tokio::test]
async fn a_single_short_page_issues_exactly_one_request() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/pulls")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            pull_list_item(7, DEPENDABOT_LOGIN),
            pull_list_item(8, DEPENDABOT_LOGIN),
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let requests = client(&server)
        .list_dependabot_prs(OWNER, REPO, REPOSITORY_ID)
        .await
        .unwrap();

    assert_eq!(
        requests.iter().map(|r| r.number).collect::<Vec<_>>(),
        [7, 8]
    );
    server.verify().await;
}

// --- merge method resolution ----------------------------------------------

/// A listed repository with the given merge settings; `None` leaves the flag
/// out of the answer, as GitHub does when the App cannot read it.
fn repository_allowing(
    id: u64,
    squash: Option<bool>,
    merge_commit: Option<bool>,
    rebase: Option<bool>,
) -> Value {
    let mut repository = repository(id);
    for (name, flag) in [
        ("allow_squash_merge", squash),
        ("allow_merge_commit", merge_commit),
        ("allow_rebase_merge", rebase),
    ] {
        if let Some(flag) = flag {
            repository[name] = json!(flag);
        }
    }
    repository
}

#[tokio::test]
async fn repository_listing_resolves_a_method_only_where_the_preferred_one_is_disallowed() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    // The client is configured to prefer squash.
    repositories_endpoint(TOKEN)
        .respond_with(repositories_page(
            6,
            vec![
                // Squash allowed: the preference stands, no override recorded.
                repository_allowing(1, Some(true), Some(false), Some(false)),
                // Squash disallowed: the first allowed of squash, merge, rebase.
                repository_allowing(2, Some(false), Some(true), Some(true)),
                repository_allowing(3, Some(false), Some(false), Some(true)),
                // Flags absent read as allowed, matching GitHub's schema default.
                repository_allowing(4, None, None, None),
                repository_allowing(5, Some(false), None, Some(false)),
                // Nothing allowed: nothing to resolve to; GitHub gets the preference.
                repository_allowing(6, Some(false), Some(false), Some(false)),
            ],
        ))
        .mount(&server)
        .await;

    let repositories = client(&server)
        .list_installation_repositories()
        .await
        .unwrap();

    assert_eq!(
        repositories
            .iter()
            .map(|repository| (repository.repository_id, repository.merge_method))
            .collect::<Vec<_>>(),
        [
            (1, None),
            (2, Some(MergeMethod::Merge)),
            (3, Some(MergeMethod::Rebase)),
            (4, None),
            (5, Some(MergeMethod::Merge)),
            (6, None),
        ]
    );
}

#[tokio::test]
async fn repository_listing_honours_a_non_default_preference() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    repositories_endpoint(TOKEN)
        .respond_with(repositories_page(
            2,
            vec![
                repository_allowing(1, Some(true), Some(true), Some(false)),
                repository_allowing(2, Some(true), Some(true), Some(true)),
            ],
        ))
        .mount(&server)
        .await;
    let config = GithubConfig {
        merge_method: MergeMethod::Rebase,
        ..config(&server)
    };

    let repositories = GithubClient::new(config)
        .unwrap()
        .list_installation_repositories()
        .await
        .unwrap();

    assert_eq!(
        repositories
            .iter()
            .map(|repository| (repository.repository_id, repository.merge_method))
            .collect::<Vec<_>>(),
        [(1, Some(MergeMethod::Squash)), (2, None)]
    );
}

// --- snapshot assembly ----------------------------------------------------

const NUMBER: u64 = 9;
const HEAD_SHA: &str = "abc123";
const CREATED_AT: &str = "2024-05-01T10:00:00Z";
const UPDATED_AT: &str = "2024-05-02T11:30:00Z";

fn unix(rfc3339: &str) -> u64 {
    u64::try_from(
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp(),
    )
    .unwrap()
}

/// An open, unmerged Dependabot pull request at [`HEAD_SHA`].
fn dependabot_pull() -> Value {
    json!({
        "title": "Bump serde from 1.0.1 to 1.0.2",
        "html_url": format!("https://github.com/{OWNER}/{REPO}/pull/{NUMBER}"),
        "state": "open",
        "user": { "login": DEPENDABOT_LOGIN },
        "head": { "sha": HEAD_SHA },
        "labels": [{ "name": "dependencies" }, { "name": "rust" }],
        "created_at": CREATED_AT,
        "updated_at": UPDATED_AT,
        "mergeable_state": "clean",
        "merged": false,
    })
}

fn pull_path() -> String {
    format!("/repos/{OWNER}/{REPO}/pulls/{NUMBER}")
}

fn pull_endpoint() -> MockBuilder {
    Mock::given(method("GET"))
        .and(path(pull_path()))
        .and(bearer_token(TOKEN))
}

/// How many requests the server saw for `method` on exactly `path`.
async fn request_count(server: &MockServer, method: &str, path: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == method && request.url.path() == path)
        .count()
}

/// Requests other than the token exchange.
async fn api_request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| !request.url.path().starts_with("/app/"))
        .count()
}

fn ok_json(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

async fn mount_pull(server: &MockServer, pull: Value) {
    pull_endpoint()
        .respond_with(ok_json(pull))
        .mount(server)
        .await;
}

const GRAPHQL_PATH: &str = "/graphql";

/// The GraphQL endpoint, matched on the installation token it was sent with.
fn graphql_endpoint() -> MockBuilder {
    Mock::given(method("POST"))
        .and(path(GRAPHQL_PATH))
        .and(bearer_token(TOKEN))
}

/// The one query a snapshot costs, addressed at [`NUMBER`] in [`OWNER`]/[`REPO`].
fn snapshot_query() -> MockBuilder {
    graphql_endpoint()
        .and(body_string_contains("query PullRequestSnapshot"))
        .and(body_partial_json(json!({
            "variables": { "owner": OWNER, "repo": REPO, "number": NUMBER }
        })))
}

/// A connection page holding `nodes`; `next` is the cursor of the page after it, if any.
fn page(nodes: Vec<Value>, next: Option<&str>) -> Value {
    json!({
        "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
        "nodes": nodes,
    })
}

fn check_run(status: &str, conclusion: Option<&str>) -> Value {
    json!({ "__typename": "CheckRun", "status": status, "conclusion": conclusion })
}

fn status_context(state: &str) -> Value {
    json!({ "__typename": "StatusContext", "state": state })
}

fn check_suite(status: &str, conclusion: Option<&str>) -> Value {
    json!({ "status": status, "conclusion": conclusion })
}

/// The head commit as GraphQL reports it: [`HEAD_SHA`], its message, the first page of
/// check runs and commit statuses under `statusCheckRollup`, and the first page of check
/// suites. A commit with no checks or statuses has a `null` rollup.
fn head_commit(message: &str, contexts: Vec<Value>, suites: Vec<Value>) -> Value {
    let rollup = if contexts.is_empty() {
        Value::Null
    } else {
        json!({ "contexts": page(contexts, None) })
    };
    json!({
        "oid": HEAD_SHA,
        "message": message,
        "statusCheckRollup": rollup,
        "checkSuites": page(suites, None),
    })
}

/// GraphQL's view of the same pull request as [`dependabot_pull`]: open, authored by the
/// Dependabot app (which GraphQL reports as the `Bot` named `dependabot`, where REST says
/// `dependabot[bot]`), at [`HEAD_SHA`] with `commit` as its head.
fn pull_request_node(commit: Value) -> Value {
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
        "commits": { "nodes": [{ "commit": commit }] },
    })
}

/// A successful GraphQL answer carrying `pull_request` under `data`.
fn snapshot_response(pull_request: Value) -> Value {
    json!({
        "data": {
            "rateLimit": { "cost": 3, "remaining": 4997 },
            "repository": { "pullRequest": pull_request },
        }
    })
}

async fn mount_snapshot(server: &MockServer, pull_request: Value) {
    snapshot_query()
        .respond_with(ok_json(snapshot_response(pull_request)))
        .mount(server)
        .await;
}

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

/// The expected record here is the one the REST-backed client produced for this pull
/// request before snapshots moved to GraphQL; a GraphQL answer describing the same pull
/// request must project to it unchanged.
#[tokio::test]
async fn snapshot_of_a_single_dependency_pull_request() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_snapshot(
        &server,
        pull_request_node(head_commit(
            "Bump serde from 1.0.1 to 1.0.2\n\n---\nupdated-dependencies:\n- dependency-name: serde\n  dependency-type: direct:production\n  update-type: version-update:semver-patch\n...",
            vec![
                check_run("COMPLETED", Some("SUCCESS")),
                status_context("SUCCESS"),
            ],
            vec![check_suite("COMPLETED", Some("SUCCESS"))],
        )),
    )
    .await;

    let before = unix_seconds();
    let record = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap()
        .expect("an open Dependabot pull request is projected");

    assert!(record.synced_at >= before && record.synced_at <= unix_seconds());
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
            synced_at: record.synced_at,
        }
    );
    assert_eq!(
        api_request_count(&server).await,
        1,
        "a snapshot is one GraphQL request"
    );
}

#[tokio::test]
async fn snapshot_of_a_grouped_pull_request_keeps_every_dependency() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    // No runs or statuses yet: only a suite that failed to start.
    let mut pull = pull_request_node(head_commit(
        "Bump the cargo group with 2 updates\n\n---\nupdated-dependencies:\n- dependency-name: tokio\n  update-type: version-update:semver-minor\n- dependency-name: serde\n  update-type: version-update:semver-major\n...",
        vec![],
        vec![check_suite("COMPLETED", Some("STARTUP_FAILURE"))],
    ));
    pull["title"] = json!("Bump the cargo group with 2 updates");
    pull["mergeStateStatus"] = json!("BEHIND");
    mount_snapshot(&server, pull).await;

    let record = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap()
        .expect("an open Dependabot pull request is projected");

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

/// Asserts that `pull_request` is not projected, and that finding out cost one request.
async fn assert_snapshot_skipped(pull_request: Value, case: &str) {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_snapshot(&server, pull_request).await;

    let record = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap();

    assert_eq!(record, None, "{case}");
    assert_eq!(api_request_count(&server).await, 1, "{case}");
}

#[tokio::test]
async fn snapshot_skips_pull_requests_not_authored_by_dependabot() {
    // A user account named `dependabot` is not the Dependabot app: only GraphQL's `Bot`
    // maps onto REST's `dependabot[bot]`.
    for author in [
        json!({ "__typename": "User", "login": "octocat" }),
        json!({ "__typename": "User", "login": "dependabot" }),
        Value::Null,
    ] {
        let mut pull = pull_request_node(head_commit("Bump serde", vec![], vec![]));
        pull["author"] = author.clone();
        assert_snapshot_skipped(pull, &format!("author {author}")).await;
    }
}

#[tokio::test]
async fn snapshot_skips_closed_pull_requests() {
    for state in ["CLOSED", "MERGED"] {
        let mut pull = pull_request_node(head_commit("Bump serde", vec![], vec![]));
        pull["state"] = json!(state);
        assert_snapshot_skipped(pull, &format!("state {state}")).await;
    }
}

/// A follow-up page of the head commit's check contexts or suites, continuing from `after`.
fn check_page_query(operation: &str, after: &str) -> MockBuilder {
    graphql_endpoint()
        .and(body_string_contains(format!("query {operation}")))
        .and(body_partial_json(json!({
            "variables": { "owner": OWNER, "repo": REPO, "sha": HEAD_SHA, "after": after }
        })))
}

/// The answer to a query addressing [`HEAD_SHA`] directly: `commit` under `object`.
fn commit_response(commit: Value) -> Value {
    json!({ "data": { "repository": { "object": commit } } })
}

#[tokio::test]
async fn check_contexts_are_read_across_pages_until_the_last_one() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let passing: Vec<Value> = (0..100)
        .map(|_| check_run("COMPLETED", Some("SUCCESS")))
        .collect();
    let mut pull = pull_request_node(head_commit(
        "Bump serde from 1.0.1 to 1.0.2",
        vec![],
        vec![],
    ));
    pull["commits"]["nodes"][0]["commit"]["statusCheckRollup"] =
        json!({ "contexts": page(passing, Some("cursor-100")) });
    mount_snapshot(&server, pull).await;
    // The one still-running check hides on the second page.
    check_page_query("CheckContextsPage", "cursor-100")
        .respond_with(ok_json(commit_response(json!({
            "statusCheckRollup": {
                "contexts": page(vec![check_run("IN_PROGRESS", None)], None)
            }
        }))))
        .expect(1)
        .mount(&server)
        .await;

    let record = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap()
        .expect("an open Dependabot pull request is projected");

    assert_eq!(record.check_status, CheckStatus::Pending);
    assert_eq!(api_request_count(&server).await, 2);
    server.verify().await;
}

#[tokio::test]
async fn check_suites_are_read_across_pages_until_the_last_one() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let passing: Vec<Value> = (0..100)
        .map(|_| check_suite("COMPLETED", Some("SUCCESS")))
        .collect();
    let mut pull = pull_request_node(head_commit(
        "Bump serde from 1.0.1 to 1.0.2",
        vec![check_run("COMPLETED", Some("SUCCESS"))],
        vec![],
    ));
    pull["commits"]["nodes"][0]["commit"]["checkSuites"] = page(passing, Some("cursor-100"));
    mount_snapshot(&server, pull).await;
    // The workflow that never started hides on the second page.
    check_page_query("CheckSuitesPage", "cursor-100")
        .respond_with(ok_json(commit_response(json!({
            "checkSuites": page(vec![check_suite("COMPLETED", Some("STARTUP_FAILURE"))], None)
        }))))
        .expect(1)
        .mount(&server)
        .await;

    let record = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap()
        .expect("an open Dependabot pull request is projected");

    assert_eq!(record.check_status, CheckStatus::Failure);
    assert_eq!(api_request_count(&server).await, 2);
    server.verify().await;
}

/// GitHub lists a pull request's commits by date, so the last listed one is not always the
/// head: a commit carrying an older date can be pushed on top. The snapshot must describe
/// the commit at `headRefOid`, fetched by SHA when the listing disagrees.
#[tokio::test]
async fn snapshot_reads_the_head_by_sha_when_the_last_listed_commit_is_not_it() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let mut older = head_commit(
        "Bump serde from 1.0.0 to 1.0.1",
        vec![check_run("COMPLETED", Some("FAILURE"))],
        vec![],
    );
    older["oid"] = json!("older999");
    mount_snapshot(&server, pull_request_node(older)).await;
    graphql_endpoint()
        .and(body_string_contains("query HeadCommit"))
        .and(body_partial_json(json!({
            "variables": { "owner": OWNER, "repo": REPO, "sha": HEAD_SHA }
        })))
        .respond_with(ok_json(commit_response(head_commit(
            "Bump serde from 1.0.1 to 1.0.2\n\n---\nupdated-dependencies:\n- dependency-name: serde\n  update-type: version-update:semver-patch\n...",
            vec![check_run("COMPLETED", Some("SUCCESS"))],
            vec![],
        ))))
        .expect(1)
        .mount(&server)
        .await;

    let record = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap()
        .expect("an open Dependabot pull request is projected");

    assert_eq!(record.head_sha, HEAD_SHA);
    assert_eq!(record.dependency.as_deref(), Some("serde"));
    assert_eq!(record.to_version.as_deref(), Some("1.0.2"));
    assert_eq!(record.check_status, CheckStatus::Success);
    assert_eq!(api_request_count(&server).await, 2);
    server.verify().await;
}

// --- mutate, verify, confirm ----------------------------------------------

const BATCH_ID: &str = "batch-1";
const STALE_SHA: &str = "def456";

fn target() -> PrTarget {
    PrTarget {
        repository_id: REPOSITORY_ID,
        owner: OWNER.to_owned(),
        repo: REPO.to_owned(),
        number: NUMBER,
        expected_sha: HEAD_SHA.to_owned(),
        title: "Bump serde from 1.0.1 to 1.0.2".to_owned(),
        html_url: format!("https://github.com/{OWNER}/{REPO}/pull/{NUMBER}"),
    }
}

fn merge_request() -> MergeRequest {
    MergeRequest {
        batch_id: BATCH_ID.to_owned(),
        target: target(),
    }
}

fn command_request() -> CommandRequest {
    CommandRequest {
        batch_id: BATCH_ID.to_owned(),
        target: target(),
        user_id: UserId::new(DASHBOARD_USER),
        command: DependabotCommand::Rebase,
    }
}

fn update_branch_request() -> UpdateBranchRequest {
    UpdateBranchRequest {
        batch_id: BATCH_ID.to_owned(),
        target: target(),
    }
}

/// A pull request whose head has moved on since the dashboard last looked.
fn stale_pull() -> Value {
    let mut pull = dependabot_pull();
    pull["head"] = json!({ "sha": STALE_SHA });
    pull
}

fn assert_stale_sha(error: &GithubError) {
    assert!(
        matches!(
            error,
            GithubError::StaleSha { expected, actual } if expected == HEAD_SHA && actual == STALE_SHA
        ),
        "expected a stale-sha rejection, got {error:?}"
    );
}

/// [`dependabot_pull`] once GitHub has merged it, as [`MERGE_SHA`].
fn merged_pull() -> Value {
    let mut pull = dependabot_pull();
    pull["merged"] = json!(true);
    pull["state"] = json!("closed");
    pull["merge_commit_sha"] = json!(MERGE_SHA);
    pull
}

/// Mounts the pull endpoint so the verify fetch sees `first` and the confirm fetch sees `then`.
async fn mount_pull_then(server: &MockServer, first: Value, then: Value) {
    pull_endpoint()
        .respond_with(ok_json(first))
        .up_to_n_times(1)
        .mount(server)
        .await;
    pull_endpoint()
        .respond_with(ok_json(then))
        .mount(server)
        .await;
}

/// Makes the mutation's `send()` fail without any response reaching the client.
///
/// The mock redirects the request back to itself; reqwest follows the loop up
/// to its redirect limit and then reports a transport error. That is the one
/// transport-level failure a mock server can produce deterministically.
fn transport_failure(path: &str) -> ResponseTemplate {
    ResponseTemplate::new(307).insert_header("location", path)
}

/// A 200 whose body is not the JSON the client expects.
fn unparsable_success() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw("<html>gateway hiccup</html>", "text/html")
}

fn merge_path() -> String {
    format!("/repos/{OWNER}/{REPO}/pulls/{NUMBER}/merge")
}

fn merge_endpoint() -> MockBuilder {
    Mock::given(method("PUT"))
        .and(path(merge_path()))
        .and(bearer_token(TOKEN))
        .and(body_json(
            json!({ "sha": HEAD_SHA, "merge_method": "squash" }),
        ))
}

const MERGED_MESSAGE: &str = "Pull Request successfully merged";
const CONFIRMED_MERGE: &str = "merged (confirmed after an ambiguous response)";
/// The commit the merge made, as GitHub names it in the merge's answer and on the
/// merged pull request alike.
const MERGE_SHA: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a0f9e8d7c6";

fn merged_response() -> ResponseTemplate {
    ok_json(json!({ "sha": MERGE_SHA, "merged": true, "message": MERGED_MESSAGE }))
}

/// The merge as the client reports it once it is known to have landed, with `detail`
/// as the words for how that came to be known.
fn merged(detail: &str) -> Merged {
    Merged {
        detail: detail.to_owned(),
        sha: Some(MERGE_SHA.to_owned()),
    }
}

#[tokio::test]
async fn merge_succeeds_when_the_head_matches() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    merge_endpoint()
        .respond_with(merged_response())
        .expect(1)
        .mount(&server)
        .await;

    let outcome = client(&server).merge(&merge_request(), None).await.unwrap();

    assert_eq!(
        outcome,
        merged(MERGED_MESSAGE),
        "the merge names the commit it made"
    );
    server.verify().await;
}

#[tokio::test]
async fn merge_uses_the_repository_method_when_it_disallows_the_configured_one() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    // The client prefers squash, but this repository's sync resolved to a
    // merge commit. Only that method is accepted here; squash would 405.
    Mock::given(method("PUT"))
        .and(path(merge_path()))
        .and(bearer_token(TOKEN))
        .and(body_json(
            json!({ "sha": HEAD_SHA, "merge_method": "merge" }),
        ))
        .respond_with(merged_response())
        .expect(1)
        .mount(&server)
        .await;

    let outcome = client(&server)
        .merge(&merge_request(), Some(MergeMethod::Merge))
        .await
        .unwrap();

    assert_eq!(outcome, merged(MERGED_MESSAGE));
    server.verify().await;
}

#[tokio::test]
async fn merge_confirms_success_after_a_transport_failure() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull_then(&server, dependabot_pull(), merged_pull()).await;
    merge_endpoint()
        .respond_with(transport_failure(&merge_path()))
        .mount(&server)
        .await;

    let outcome = client(&server).merge(&merge_request(), None).await.unwrap();

    assert_eq!(
        outcome,
        merged(CONFIRMED_MERGE),
        "the merge confirmed on the pull request names the commit the pull request does"
    );
}

#[tokio::test]
async fn merge_confirms_success_after_an_unparsable_response() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull_then(&server, dependabot_pull(), merged_pull()).await;
    merge_endpoint()
        .respond_with(unparsable_success())
        .expect(1)
        .mount(&server)
        .await;

    let outcome = client(&server).merge(&merge_request(), None).await.unwrap();

    assert_eq!(outcome, merged(CONFIRMED_MERGE));
    server.verify().await;
}

#[tokio::test]
async fn merge_reports_the_transport_failure_when_the_confirm_shows_no_merge() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    merge_endpoint()
        .respond_with(transport_failure(&merge_path()))
        .mount(&server)
        .await;

    let error = client(&server)
        .merge(&merge_request(), None)
        .await
        .unwrap_err();

    assert!(
        matches!(error, GithubError::Transport(_)),
        "expected a transport error, got {error:?}"
    );
    assert_eq!(
        request_count(&server, "GET", &pull_path()).await,
        2,
        "the client re-fetched the pull request to check for a merge"
    );
}

#[tokio::test]
async fn merge_rejects_a_stale_head_without_calling_github() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, stale_pull()).await;
    Mock::given(method("PUT"))
        .and(path(merge_path()))
        .respond_with(merged_response())
        .expect(0)
        .mount(&server)
        .await;

    let error = client(&server)
        .merge(&merge_request(), None)
        .await
        .unwrap_err();

    assert_stale_sha(&error);
    server.verify().await;
}

#[tokio::test]
async fn merge_is_a_no_op_when_github_already_merged_the_pull_request() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, merged_pull()).await;
    Mock::given(method("PUT"))
        .and(path(merge_path()))
        .respond_with(merged_response())
        .expect(0)
        .mount(&server)
        .await;

    let outcome = client(&server).merge(&merge_request(), None).await.unwrap();

    assert_eq!(
        outcome,
        merged("already merged"),
        "a merge found already done still names the commit"
    );
    server.verify().await;
}

/// GitHub's `merge_commit_sha` is nullable. A pull request found merged without one
/// is still a merge that happened; the client says so, with no commit to name.
#[tokio::test]
async fn merge_found_already_done_without_a_commit_named_reports_the_merge_without_one() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    let mut pull = merged_pull();
    pull["merge_commit_sha"] = Value::Null;
    mount_pull(&server, pull).await;

    let outcome = client(&server).merge(&merge_request(), None).await.unwrap();

    assert_eq!(
        outcome,
        Merged {
            detail: "already merged".to_owned(),
            sha: None,
        }
    );
}

#[tokio::test]
async fn merge_rejection_from_github_is_reported_against_the_known_pull_request() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    merge_endpoint()
        .respond_with(github_error(405, "Pull Request is not mergeable"))
        .expect(1)
        .mount(&server)
        .await;

    let error = client(&server)
        .merge(&merge_request(), None)
        .await
        .unwrap_err();

    assert!(
        matches!(
            &error,
            GithubError::Http {
                response,
                known_resource: true
            } if response.status == 405
        ),
        "the pull request was verified to exist, got {error:?}"
    );
    server.verify().await;
}

// --- Dependabot commands ---------------------------------------------------

fn comments_path() -> String {
    format!("/repos/{OWNER}/{REPO}/issues/{NUMBER}/comments")
}

fn marker() -> String {
    format!("<!-- dependaboard-batch:{BATCH_ID} -->")
}

/// One page of the comment listing the client scans for its idempotency
/// marker. Pinning the page keeps a pagination regression from spinning.
fn comment_page(page: u32) -> MockBuilder {
    Mock::given(method("GET"))
        .and(path(comments_path()))
        .and(bearer_token(TOKEN))
        .and(query_param("per_page", "100"))
        .and(query_param("page", page.to_string()))
}

/// Posting the command as the dashboard user: Dependabot only obeys humans.
fn comment_post() -> MockBuilder {
    Mock::given(method("POST"))
        .and(path(comments_path()))
        .and(bearer_token(USER_PAT))
        .and(body_string_contains("@dependabot rebase"))
        .and(body_string_contains(marker()))
}

fn marked_comment(id: u64) -> Value {
    json!({ "id": id, "body": format!("@dependabot rebase\n\n{}", marker()) })
}

#[tokio::test]
async fn command_is_posted_as_the_dashboard_user_when_no_marker_exists() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    comment_page(1)
        .respond_with(ok_json(json!([{ "id": 1, "body": "unrelated" }])))
        .expect(1)
        .mount(&server)
        .await;
    comment_post()
        .respond_with(ResponseTemplate::new(201).set_body_json(marked_comment(555)))
        .expect(1)
        .mount(&server)
        .await;

    let detail = client(&server)
        .post_command(&command_request())
        .await
        .unwrap();

    assert_eq!(detail, "GitHub accepted comment #555");
    server.verify().await;
}

#[tokio::test]
async fn command_confirms_success_after_a_transport_failure() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    comment_page(1)
        .respond_with(ok_json(json!([])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    comment_page(1)
        .respond_with(ok_json(json!([marked_comment(777)])))
        .mount(&server)
        .await;
    comment_post()
        .respond_with(transport_failure(&comments_path()))
        .mount(&server)
        .await;

    let detail = client(&server)
        .post_command(&command_request())
        .await
        .unwrap();

    assert_eq!(
        detail,
        "command accepted as comment #777 (confirmed after an ambiguous response)"
    );
}

#[tokio::test]
async fn command_confirms_success_after_an_unparsable_response() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    comment_page(1)
        .respond_with(ok_json(json!([])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    comment_page(1)
        .respond_with(ok_json(json!([marked_comment(777)])))
        .mount(&server)
        .await;
    comment_post()
        .respond_with(unparsable_success())
        .expect(1)
        .mount(&server)
        .await;

    let detail = client(&server)
        .post_command(&command_request())
        .await
        .unwrap();

    assert_eq!(
        detail,
        "command accepted as comment #777 (confirmed after an ambiguous response)"
    );
    server.verify().await;
}

/// A merge-only deployment mints no PAT. The client says so up front, so the service can
/// reject a rebase rather than attempt it, and a command that slips through is refused
/// with the variable to set before GitHub is asked anything: not even the verify read.
#[tokio::test]
async fn without_a_user_pat_the_client_cannot_post_commands_and_refuses_them_untouched() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok_json(json!({})))
        .expect(0)
        .mount(&server)
        .await;
    let config = GithubConfig {
        user_pat: None,
        ..config(&server)
    };
    let merge_only = GithubClient::new(config).expect("a PAT is not needed to build a client");

    assert!(client(&server).can_post_commands());
    assert!(!merge_only.can_post_commands());
    let error = merge_only
        .post_command(&command_request())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, GithubError::Config(message) if message.contains("GITHUB_USER_PAT")),
        "expected a config error naming the variable to set, got {error:?}"
    );
    server.verify().await;
}

#[tokio::test]
async fn command_that_cannot_be_sent_is_not_read_back() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    comment_page(1)
        .respond_with(ok_json(json!([])))
        .mount(&server)
        .await;
    // Only the configured dashboard user has a token; the request comes from someone else.
    let config = GithubConfig {
        dashboard_user: UserId::new("someone-else"),
        ..config(&server)
    };

    let error = GithubClient::new(config)
        .expect("test config builds a client")
        .post_command(&command_request())
        .await
        .unwrap_err();

    assert!(
        matches!(&error, GithubError::Config(message) if message.contains(DASHBOARD_USER)),
        "expected the missing-token config error, got {error:?}"
    );
    assert_eq!(
        request_count(&server, "GET", &comments_path()).await,
        1,
        "nothing reached GitHub, so there was nothing to read back"
    );
    assert_eq!(request_count(&server, "POST", &comments_path()).await, 0);
}

#[tokio::test]
async fn command_rejects_a_stale_head_without_touching_comments() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, stale_pull()).await;
    Mock::given(path(comments_path()))
        .respond_with(ok_json(json!([])))
        .expect(0)
        .mount(&server)
        .await;

    let error = client(&server)
        .post_command(&command_request())
        .await
        .unwrap_err();

    assert_stale_sha(&error);
    server.verify().await;
}

#[tokio::test]
async fn command_is_not_reposted_when_its_marker_is_already_present() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    comment_page(1)
        .respond_with(ok_json(json!([marked_comment(321)])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(comments_path()))
        .respond_with(ResponseTemplate::new(201).set_body_json(marked_comment(999)))
        .expect(0)
        .mount(&server)
        .await;

    let detail = client(&server)
        .post_command(&command_request())
        .await
        .unwrap();

    assert_eq!(detail, "command already accepted as comment #321");
    server.verify().await;
}

#[tokio::test]
async fn marker_scan_reads_past_a_full_first_page_and_stops_on_the_short_one() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    let unrelated: Vec<Value> = (1..=100)
        .map(|id| json!({ "id": id, "body": "unrelated" }))
        .collect();
    comment_page(1)
        .respond_with(ok_json(json!(unrelated)))
        .expect(1)
        .mount(&server)
        .await;
    comment_page(2)
        .respond_with(ok_json(json!([marked_comment(4242)])))
        .expect(1)
        .mount(&server)
        .await;

    let detail = client(&server)
        .post_command(&command_request())
        .await
        .unwrap();

    assert_eq!(detail, "command already accepted as comment #4242");
    assert_eq!(request_count(&server, "GET", &comments_path()).await, 2);
    assert_eq!(request_count(&server, "POST", &comments_path()).await, 0);
    server.verify().await;
}

// --- update branch --------------------------------------------------------

fn update_branch_path() -> String {
    format!("/repos/{OWNER}/{REPO}/pulls/{NUMBER}/update-branch")
}

fn update_branch_endpoint() -> MockBuilder {
    Mock::given(method("PUT"))
        .and(path(update_branch_path()))
        .and(bearer_token(TOKEN))
        .and(body_json(json!({ "expected_head_sha": HEAD_SHA })))
}

const UPDATING_MESSAGE: &str = "Updating pull request branch.";

fn updating_response() -> ResponseTemplate {
    ResponseTemplate::new(202).set_body_json(json!({
        "message": UPDATING_MESSAGE,
        "url": format!("https://api.github.com/repos/{OWNER}/{REPO}/pulls/{NUMBER}"),
    }))
}

#[tokio::test]
async fn update_branch_succeeds_when_the_head_matches() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    update_branch_endpoint()
        .respond_with(updating_response())
        .expect(1)
        .mount(&server)
        .await;

    let detail = client(&server)
        .update_branch(&update_branch_request())
        .await
        .unwrap();

    assert_eq!(detail, UPDATING_MESSAGE);
    server.verify().await;
}

#[tokio::test]
async fn update_branch_confirms_success_when_the_head_moved_after_a_transport_failure() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    // The verify fetch sees the expected head; the confirm fetch sees GitHub's new one.
    mount_pull_then(&server, dependabot_pull(), stale_pull()).await;
    update_branch_endpoint()
        .respond_with(transport_failure(&update_branch_path()))
        .mount(&server)
        .await;

    let detail = client(&server)
        .update_branch(&update_branch_request())
        .await
        .unwrap();

    assert_eq!(
        detail,
        "branch updated (confirmed after an ambiguous response)"
    );
}

#[tokio::test]
async fn update_branch_confirms_success_when_the_head_moved_after_an_unparsable_response() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull_then(&server, dependabot_pull(), stale_pull()).await;
    update_branch_endpoint()
        .respond_with(unparsable_success())
        .expect(1)
        .mount(&server)
        .await;

    let detail = client(&server)
        .update_branch(&update_branch_request())
        .await
        .unwrap();

    assert_eq!(
        detail,
        "branch updated (confirmed after an ambiguous response)"
    );
    server.verify().await;
}

#[tokio::test]
async fn update_branch_reports_the_transport_failure_when_the_head_did_not_move() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    update_branch_endpoint()
        .respond_with(transport_failure(&update_branch_path()))
        .mount(&server)
        .await;

    let error = client(&server)
        .update_branch(&update_branch_request())
        .await
        .unwrap_err();

    assert!(
        matches!(error, GithubError::Transport(_)),
        "expected a transport error, got {error:?}"
    );
    assert_eq!(
        request_count(&server, "GET", &pull_path()).await,
        2,
        "the client re-fetched the pull request to check whether the head moved"
    );
}

#[tokio::test]
async fn update_branch_rejects_a_stale_head_without_calling_github() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, stale_pull()).await;
    Mock::given(method("PUT"))
        .and(path(update_branch_path()))
        .respond_with(updating_response())
        .expect(0)
        .mount(&server)
        .await;

    let error = client(&server)
        .update_branch(&update_branch_request())
        .await
        .unwrap_err();

    assert_stale_sha(&error);
    server.verify().await;
}

// --- error responses ------------------------------------------------------

#[tokio::test]
async fn rate_limit_headers_are_parsed_from_error_responses() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    snapshot_query()
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "1700000000")
                .insert_header("retry-after", "30")
                .set_body_json(json!({
                    "message": "API rate limit exceeded",
                    "documentation_url": "https://docs.github.com/rest/overview/rate-limits",
                })),
        )
        .mount(&server)
        .await;

    let error = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap_err();

    assert_eq!(
        http_response(&error).cloned().expect("an HTTP error"),
        GithubErrorResponse {
            status: 403,
            message: "API rate limit exceeded".to_owned(),
            documentation_url: Some("https://docs.github.com/rest/overview/rate-limits".to_owned()),
            rate_limit_remaining: Some(0),
            rate_limit_reset: Some(1_700_000_000),
            retry_after_seconds: Some(30),
        }
    );
    // A read that fails at the first fetch has not proven the resource exists.
    assert!(
        matches!(
            error,
            GithubError::Http {
                known_resource: false,
                ..
            }
        ),
        "got {error:?}"
    );
}

/// GitHub's primary GraphQL limit does not fail the HTTP request: the answer is a `200`
/// carrying a `RATE_LIMITED` error, and only its headers say when the limit resets.
#[tokio::test]
async fn a_rate_limited_graphql_answer_is_an_http_error_with_its_headers() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    snapshot_query()
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "1700000000")
                .set_body_json(json!({
                    "data": null,
                    "errors": [{
                        "type": "RATE_LIMITED",
                        "message": "API rate limit exceeded for installation ID 42.",
                    }],
                })),
        )
        .mount(&server)
        .await;

    let error = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap_err();

    assert_eq!(
        http_response(&error).cloned().expect("an HTTP error"),
        GithubErrorResponse {
            status: 429,
            message: "API rate limit exceeded for installation ID 42.".to_owned(),
            documentation_url: None,
            rate_limit_remaining: Some(0),
            rate_limit_reset: Some(1_700_000_000),
            retry_after_seconds: None,
        }
    );
}

/// A pull request GraphQL cannot resolve reads exactly like a REST 404, so the caller's
/// "gone if it was known, misconfigured if not" logic is unchanged.
#[tokio::test]
async fn an_unresolvable_pull_request_reads_as_not_found() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    snapshot_query()
        .respond_with(ok_json(json!({
            "data": { "repository": { "pullRequest": null } },
            "errors": [{
                "type": "NOT_FOUND",
                "path": ["repository", "pullRequest"],
                "message": "Could not resolve to a PullRequest with the number of 9.",
            }],
        })))
        .mount(&server)
        .await;

    let error = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap_err();

    let response = http_response(&error).expect("an HTTP error");
    assert_eq!(response.status, 404);
    assert_eq!(
        response.message,
        "Could not resolve to a PullRequest with the number of 9."
    );
    assert_eq!(response.rate_limit_remaining, None);
    assert_eq!(response.rate_limit_reset, None);
    assert_eq!(response.retry_after_seconds, None);
    assert!(
        matches!(
            error,
            GithubError::Http {
                known_resource: false,
                ..
            }
        ),
        "got {error:?}"
    );
}

#[tokio::test]
async fn missing_rate_limit_headers_are_left_unset() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    snapshot_query()
        .respond_with(github_error(404, "Not Found"))
        .mount(&server)
        .await;

    let error = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap_err();

    let response = http_response(&error).expect("an HTTP error");
    assert_eq!(response.status, 404);
    assert_eq!(response.message, "Not Found");
    assert_eq!(response.documentation_url, None);
    assert_eq!(response.rate_limit_remaining, None);
    assert_eq!(response.rate_limit_reset, None);
    assert_eq!(response.retry_after_seconds, None);
}

#[tokio::test]
async fn unparsable_error_bodies_still_carry_the_status_and_headers() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    snapshot_query()
        .respond_with(
            ResponseTemplate::new(502)
                .insert_header("retry-after", "5")
                .set_body_raw("<html>Bad Gateway</html>", "text/html"),
        )
        .mount(&server)
        .await;

    let error = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap_err();

    let response = http_response(&error).expect("an HTTP error");
    assert_eq!(response.status, 502);
    assert_eq!(response.message, "");
    assert_eq!(response.retry_after_seconds, Some(5));
}

// --- error sources --------------------------------------------------------

/// The `source()` chain below `error`, outermost first, for inspecting what a
/// client error wraps without caring how many layers deep it sits.
fn source_chain(error: &GithubError) -> Vec<&(dyn std::error::Error + 'static)> {
    let mut chain = Vec::new();
    let mut current = std::error::Error::source(error);
    while let Some(source) = current {
        chain.push(source);
        current = source.source();
    }
    chain
}

#[tokio::test]
async fn transport_failures_keep_the_underlying_client_error_as_their_source() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    merge_endpoint()
        .respond_with(transport_failure(&merge_path()))
        .mount(&server)
        .await;

    let error = client(&server)
        .merge(&merge_request(), None)
        .await
        .unwrap_err();

    assert!(
        matches!(error, GithubError::Transport(_)),
        "expected a transport error, got {error:?}"
    );
    assert!(
        source_chain(&error)
            .iter()
            .any(|source| source.is::<reqwest::Error>()),
        "the reqwest error should be reachable through source(), got {error:?}"
    );
}

#[tokio::test]
async fn merge_reports_an_ambiguous_outcome_when_an_unreadable_answer_is_not_confirmed() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    mount_pull(&server, dependabot_pull()).await;
    merge_endpoint()
        .respond_with(unparsable_success())
        .expect(1)
        .mount(&server)
        .await;

    let error = client(&server)
        .merge(&merge_request(), None)
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            GithubError::Ambiguous {
                operation: Operation::Merge,
                ..
            }
        ),
        "expected an ambiguous merge outcome, got {error:?}"
    );
    let chain = source_chain(&error);
    assert!(
        chain.iter().any(|source| source.is::<ProtocolError>()),
        "the unreadable answer should be the ambiguity's source, got {error:?}"
    );
    assert!(
        chain.iter().any(|source| source.is::<reqwest::Error>()),
        "the decode error should be reachable through source(), got {error:?}"
    );
    assert_eq!(
        request_count(&server, "GET", &pull_path()).await,
        2,
        "the client re-fetched the pull request to check for a merge"
    );
    server.verify().await;
}

#[tokio::test]
async fn unreadable_answers_keep_the_decode_error_as_their_source() {
    let server = MockServer::start().await;
    mount_token(&server).await;
    snapshot_query()
        .respond_with(unparsable_success())
        .mount(&server)
        .await;

    let error = client(&server)
        .fetch_snapshot(&sync_request())
        .await
        .unwrap_err();

    assert!(
        matches!(error, GithubError::Protocol(ProtocolError::Body(_))),
        "expected an unreadable-body protocol error, got {error:?}"
    );
    assert!(
        source_chain(&error)
            .iter()
            .any(|source| source.is::<reqwest::Error>()),
        "the decode error should be reachable through source(), got {error:?}"
    );
}
