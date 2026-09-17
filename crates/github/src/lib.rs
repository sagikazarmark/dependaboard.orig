use std::{collections::HashMap, env, fs, sync::Arc, time::Duration};

use async_trait::async_trait;
use dependaboard_core::{
    CheckSignal, CommandRequest, DEPENDABOT_LOGIN, GithubErrorResponse, MergeMethod, MergeRequest,
    Operation, PrRecord, PrTarget, RepoRecord, SyncRequest, UpdateBranchRequest, UserId,
    unix_seconds,
};
use jsonwebtoken::EncodingKey;
use reqwest::{Method, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;

mod auth;
mod error;
mod graphql;
mod rest;

pub use auth::TokenProvider;
pub use error::{GithubError, ProtocolError};

use crate::{
    auth::{CachedToken, StaticTokenProvider, no_user_token},
    error::{known_http, not_found},
    graphql::{
        CHECK_CONTEXTS_PAGE_QUERY, CHECK_SUITES_PAGE_QUERY, CheckContextsPage, CheckSuitesPage,
        CommitData, Connection, GraphqlResponse, SnapshotData, check_context_signal,
        check_suite_signal, head_commit_query, project_snapshot, snapshot_query,
    },
    rest::{
        GithubPull, InstallationRepositories, IssueComment, MergeResult, PullListItem,
        RateLimitHeaders, UpdateBranchResult, fold_repository_pages, http_error, parse_response,
    },
};

const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "dependaboard/0.1";
/// GitHub's largest page for the listings paged here.
const LISTING_PAGE_SIZE: usize = 100;

#[derive(Clone)]
pub struct GithubConfig {
    pub api_url: String,
    pub app_id: u64,
    pub installation_id: u64,
    pub private_key: SecretString,
    /// The user identity `@dependabot` commands are posted under. Merge and update
    /// branch run as the App, so a merge-only deployment leaves it unset and the
    /// client refuses to post commands.
    pub user_pat: Option<SecretString>,
    pub dashboard_user: UserId,
    pub merge_method: MergeMethod,
}

impl GithubConfig {
    pub fn from_env() -> Result<Self, GithubError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    /// Builds the config from a variable lookup, so the parsing rules can be
    /// exercised without touching the process environment.
    fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Result<Self, GithubError> {
        let private_key = match var("GITHUB_PRIVATE_KEY") {
            Some(value) => value.replace("\\n", "\n"),
            None => {
                let path = var("GITHUB_PRIVATE_KEY_PATH").ok_or_else(|| {
                    GithubError::Config(
                        "set GITHUB_PRIVATE_KEY or GITHUB_PRIVATE_KEY_PATH".to_owned(),
                    )
                })?;
                fs::read_to_string(path).map_err(|error| {
                    GithubError::Config(format!("cannot read private key: {error}"))
                })?
            }
        };
        let dashboard_user = var("DASHBOARD_USERNAME").unwrap_or_else(|| "dependaboard".to_owned());
        if dashboard_user.trim().is_empty() {
            return Err(GithubError::Config(
                "DASHBOARD_USERNAME must not be empty".to_owned(),
            ));
        }
        let merge_method = var("GITHUB_MERGE_METHOD")
            .unwrap_or_else(|| MergeMethod::default().to_string())
            .parse()
            .map_err(|_| {
                GithubError::Config(
                    "GITHUB_MERGE_METHOD must be merge, squash, or rebase".to_owned(),
                )
            })?;
        Ok(Self {
            api_url: var("GITHUB_API_URL")
                .unwrap_or_else(|| "https://api.github.com".to_owned())
                .trim_end_matches('/')
                .to_owned(),
            app_id: parse_u64_var(&var, "GITHUB_APP_ID")?,
            installation_id: parse_u64_var(&var, "GITHUB_INSTALLATION_ID")?,
            private_key: SecretString::from(private_key),
            user_pat: var("GITHUB_USER_PAT")
                .filter(|value| !value.trim().is_empty())
                .map(SecretString::from),
            dashboard_user: UserId::new(dashboard_user),
            merge_method,
        })
    }

    /// Where the GraphQL API answers, derived from the REST root: `/graphql` beside
    /// `api.github.com`'s root, and `/api/graphql` beside GitHub Enterprise's `/api/v3`.
    fn graphql_url(&self) -> String {
        match self.api_url.strip_suffix("/api/v3") {
            Some(host) => format!("{host}/api/graphql"),
            None => format!("{}/graphql", self.api_url),
        }
    }
}

fn parse_u64_var(var: &impl Fn(&str) -> Option<String>, name: &str) -> Result<u64, GithubError> {
    var(name)
        .ok_or_else(|| GithubError::Config(format!("set {name}")))?
        .parse()
        .map_err(|_| GithubError::Config(format!("{name} must be an unsigned integer")))
}

#[derive(Clone)]
pub struct GithubClient {
    config: Arc<GithubConfig>,
    /// The App's RSA key, parsed once at construction so a malformed key is a
    /// startup failure rather than a surprise on the first API call.
    app_key: EncodingKey,
    http: reqwest::Client,
    tokens: Arc<Mutex<HashMap<u64, CachedToken>>>,
    user_tokens: Arc<dyn TokenProvider>,
    /// Whether `user_tokens` can answer for anyone: false for a deployment without a
    /// PAT, where every command is refused before GitHub is asked anything.
    user_commands: bool,
}

impl GithubClient {
    /// A client for the configured deployment: the dashboard user's PAT is the one user
    /// token, and without one commands are refused.
    pub fn new(config: GithubConfig) -> Result<Self, GithubError> {
        let user_commands = config.user_pat.is_some();
        let user_tokens = Arc::new(StaticTokenProvider {
            user: config.dashboard_user.clone(),
            token: config.user_pat.clone(),
        });
        Self::build(config, user_tokens, user_commands)
    }

    /// A client whose user tokens come from `user_tokens`, which is taken to have a
    /// token for the users it serves: commands are offered.
    pub fn with_token_provider(
        config: GithubConfig,
        user_tokens: Arc<dyn TokenProvider>,
    ) -> Result<Self, GithubError> {
        Self::build(config, user_tokens, true)
    }

    fn build(
        config: GithubConfig,
        user_tokens: Arc<dyn TokenProvider>,
        user_commands: bool,
    ) -> Result<Self, GithubError> {
        let app_key = EncodingKey::from_rsa_pem(config.private_key.expose_secret().as_bytes())
            .map_err(|error| GithubError::Config(format!("invalid GitHub private key: {error}")))?;
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(GithubError::Transport)?;
        Ok(Self {
            config: Arc::new(config),
            app_key,
            http,
            tokens: Arc::new(Mutex::new(HashMap::new())),
            user_tokens,
            user_commands,
        })
    }

    pub fn installation_id(&self) -> u64 {
        self.config.installation_id
    }

    async fn installation_request(
        &self,
        installation_id: u64,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Response, GithubError> {
        let url = format!("{}{}", self.config.api_url, path);
        self.installation_send(installation_id, method, &url, body)
            .await
    }

    /// Sends one request as the installation, refreshing the token once on a 401.
    async fn installation_send(
        &self,
        installation_id: u64,
        method: Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Response, GithubError> {
        for attempt in 0..2 {
            let token = self
                .installation_token(installation_id, attempt > 0)
                .await?;
            let mut request = self
                .http
                .request(method.clone(), url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION)
                .bearer_auth(token.expose_secret());
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(GithubError::Transport)?;
            if response.status() != StatusCode::UNAUTHORIZED || attempt == 1 {
                return Ok(response);
            }
        }
        unreachable!("the authorization loop always returns")
    }

    /// Runs one GraphQL query as the installation and reads its `data`.
    ///
    /// A non-success status is the same HTTP error a REST call would raise. Inside a
    /// success, GraphQL reports failures as `errors`, which are read before `data` and
    /// folded into [`GithubError::Http`] with the status the REST API would have used
    /// (see [`graphql::error_status`]), so callers classify both APIs' failures alike.
    /// The rate-limit headers are read either way: GitHub's primary GraphQL limit
    /// arrives as a `RATE_LIMITED` error inside a `200` whose headers carry the reset.
    async fn graphql_query<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<T, GithubError> {
        let response = self
            .installation_send(
                self.config.installation_id,
                Method::POST,
                &self.config.graphql_url(),
                Some(&json!({ "query": query, "variables": variables })),
            )
            .await?;
        if !response.status().is_success() {
            return Err(GithubError::Http {
                response: http_error(response).await,
                known_resource: false,
            });
        }
        let rate_limit = RateLimitHeaders::read(&response);
        let answer: GraphqlResponse<T> = response
            .json()
            .await
            .map_err(|error| GithubError::Protocol(ProtocolError::Body(error)))?;
        if !answer.errors.is_empty() {
            return Err(GithubError::Http {
                response: graphql::error_response(&answer.errors, rate_limit),
                known_resource: false,
            });
        }
        answer
            .data
            .ok_or(GithubError::Protocol(ProtocolError::Missing("data")))
    }

    async fn user_request(
        &self,
        user: &UserId,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Response, GithubError> {
        let token = self.user_tokens.user_token(user).await?;
        let mut request = self
            .http
            .request(method, format!("{}{}", self.config.api_url, path))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(token.expose_secret());
        if let Some(body) = body {
            request = request.json(body);
        }
        request.send().await.map_err(GithubError::Transport)
    }

    async fn installation_json<T: DeserializeOwned>(
        &self,
        installation_id: u64,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, GithubError> {
        parse_response(
            self.installation_request(installation_id, method, path, body)
                .await?,
        )
        .await
    }

    async fn fetch_pull(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<GithubPull, GithubError> {
        self.installation_json(
            installation_id,
            Method::GET,
            &format!("/repos/{owner}/{repo}/pulls/{number}"),
            None,
        )
        .await
    }

    /// Reads the pull request's GraphQL snapshot, whatever its state or author; the
    /// caller decides whether it is one the dashboard projects.
    async fn fetch_pull_request_node(
        &self,
        request: &SyncRequest,
    ) -> Result<graphql::PullRequestNode, GithubError> {
        let data: SnapshotData = self
            .graphql_query(
                &snapshot_query(),
                json!({
                    "owner": request.owner,
                    "repo": request.repo,
                    "number": request.number,
                }),
            )
            .await?;
        if let Some(rate_limit) = &data.rate_limit {
            tracing::debug!(
                owner = %request.owner,
                repo = %request.repo,
                number = request.number,
                cost = rate_limit.cost,
                remaining = rate_limit.remaining,
                "fetched pull request snapshot over GraphQL"
            );
        }
        data.repository
            .and_then(|repository| repository.pull_request)
            .ok_or(GithubError::Protocol(ProtocolError::Missing(
                "pull request",
            )))
    }

    /// The pull request's head commit: the one GitHub lists last, unless that is not the
    /// commit at `headRefOid`, in which case the head is read by SHA.
    async fn fetch_head_commit(
        &self,
        request: &SyncRequest,
        pull: &graphql::PullRequestNode,
    ) -> Result<graphql::Commit, GithubError> {
        if let Some(listed) = pull.last_listed_commit()
            && listed.oid == pull.head_ref_oid
        {
            return Ok(listed.clone());
        }
        self.commit_by_sha(&head_commit_query(), request, &pull.head_ref_oid, None)
            .await?
            .ok_or(GithubError::Protocol(ProtocolError::Missing("head commit")))
    }

    /// Runs a query that addresses the commit at `sha` in the pull request's repository,
    /// continuing a paged connection from `after` when given.
    async fn commit_by_sha<T: DeserializeOwned>(
        &self,
        query: &str,
        request: &SyncRequest,
        sha: &str,
        after: Option<&str>,
    ) -> Result<Option<T>, GithubError> {
        let data: CommitData<T> = self
            .graphql_query(
                query,
                json!({
                    "owner": request.owner,
                    "repo": request.repo,
                    "sha": sha,
                    "after": after,
                }),
            )
            .await?;
        Ok(data.into_commit())
    }

    /// Rolls the head commit's check runs, commit statuses, and check suites up into
    /// signals, following every page so a failure past the first hundred still counts.
    async fn check_signals(
        &self,
        request: &SyncRequest,
        commit: &graphql::Commit,
    ) -> Result<Vec<CheckSignal>, GithubError> {
        let sha = &commit.oid;
        let mut signals = Vec::new();
        if let Some(rollup) = &commit.status_check_rollup {
            signals.extend(
                signals_across_pages(&rollup.contexts, check_context_signal, |after| async move {
                    Ok(self
                        .commit_by_sha::<CheckContextsPage>(
                            CHECK_CONTEXTS_PAGE_QUERY,
                            request,
                            sha,
                            Some(&after),
                        )
                        .await?
                        .and_then(|commit| commit.status_check_rollup)
                        .map(|rollup| rollup.contexts))
                })
                .await?,
            );
        }
        if let Some(suites) = &commit.check_suites {
            signals.extend(
                signals_across_pages(suites, check_suite_signal, |after| async move {
                    Ok(self
                        .commit_by_sha::<CheckSuitesPage>(
                            CHECK_SUITES_PAGE_QUERY,
                            request,
                            sha,
                            Some(&after),
                        )
                        .await?
                        .and_then(|commit| commit.check_suites))
                })
                .await?,
            );
        }
        Ok(signals)
    }

    async fn find_comment_with_marker(
        &self,
        path: &str,
        marker: &str,
    ) -> Result<Option<u64>, GithubError> {
        let mut page = 1;
        loop {
            let comments: Vec<IssueComment> = parse_response(
                self.installation_request(
                    self.config.installation_id,
                    Method::GET,
                    &format!("{path}?per_page={LISTING_PAGE_SIZE}&page={page}"),
                    None,
                )
                .await?,
            )
            .await?;
            if let Some(comment) = comments
                .iter()
                .find(|comment| comment.body.contains(marker))
            {
                return Ok(Some(comment.id));
            }
            if comments.len() < LISTING_PAGE_SIZE {
                return Ok(None);
            }
            page += 1;
        }
    }

    /// Reads the pull request a mutation targets.
    async fn fetch_target(&self, target: &PrTarget) -> Result<GithubPull, GithubError> {
        self.fetch_pull(
            self.config.installation_id,
            &target.owner,
            &target.repo,
            target.number,
        )
        .await
    }

    /// Runs one GitHub write against a verified pull request: fetch, verify, mutate, confirm.
    ///
    /// The pull request is read and checked to still be the open Dependabot pull request at
    /// the head the caller last saw; only then does `mutate` send the write.
    ///
    /// GitHub mutations are at-least-once, so a replay must find work an earlier attempt
    /// finished and not redo it. There are two places to notice that, and they sit on
    /// opposite sides of the checks: `already_applied` sees the verify read itself, before
    /// the state checks, because a merge that landed has closed the pull request and would
    /// otherwise be rejected as gone; `mutate` may instead answer [`Write::AlreadyApplied`]
    /// after the checks, for work that costs extra requests to detect (the comment marker
    /// scan) and must not be spent on a pull request that is about to be rejected as stale.
    ///
    /// A lost answer (transport failure) or an unreadable one (a 2xx whose body does not
    /// parse) is not yet a failure: `confirm` reads back whether the write landed and, when
    /// it did, the mutation succeeds with what the read-back says of it. Otherwise a
    /// transport failure stands and an unreadable answer becomes [`GithubError::Ambiguous`].
    /// Any other failure to send means nothing reached GitHub and is returned as is. A
    /// read-back that itself fails counts as unconfirmed.
    ///
    /// `A` is what the caller makes of a write known to have landed without GitHub's
    /// answer to read: the words for it, and whatever else the read-back has to say — a
    /// merge found done names its commit. `T` is GitHub's answer when there is one.
    ///
    /// HTTP errors GitHub returns after the pull request was verified are flagged as coming
    /// from a known resource.
    async fn verified_mutation<T: DeserializeOwned, A>(
        &self,
        operation: Operation,
        target: &PrTarget,
        already_applied: impl FnOnce(&GithubPull) -> Option<A>,
        mutate: impl AsyncFnOnce() -> Result<Write<A>, GithubError>,
        confirm: impl AsyncFnOnce() -> Result<Option<A>, GithubError>,
    ) -> Result<MutationOutcome<T, A>, GithubError> {
        let pull = self.fetch_target(target).await?;
        if let Some(applied) = already_applied(&pull) {
            return Ok(MutationOutcome::Applied(applied));
        }
        if pull.state != "open" || pull.user.login != DEPENDABOT_LOGIN {
            return Err(not_found(
                "pull request is closed or is not owned by Dependabot",
            ));
        }
        if pull.head.sha != target.expected_sha {
            return Err(GithubError::StaleSha {
                expected: target.expected_sha.clone(),
                actual: pull.head.sha,
            });
        }
        let response = match mutate().await {
            Ok(Write::Sent(response)) => response,
            Ok(Write::AlreadyApplied(applied)) => return Ok(MutationOutcome::Applied(applied)),
            Err(lost @ GithubError::Transport(_)) => {
                return match confirm().await {
                    Ok(Some(applied)) => Ok(MutationOutcome::Applied(applied)),
                    Ok(None) | Err(_) => Err(lost),
                };
            }
            Err(error) => return Err(error),
        };
        match parse_response(response).await.map_err(known_http) {
            Ok(answer) => Ok(MutationOutcome::Answered(answer)),
            Err(GithubError::Protocol(unreadable)) => match confirm().await {
                Ok(Some(applied)) => Ok(MutationOutcome::Applied(applied)),
                Ok(None) | Err(_) => Err(GithubError::Ambiguous {
                    operation,
                    source: unreadable,
                }),
            },
            Err(error) => Err(error),
        }
    }

    /// The read-back for writes whose effect shows on the pull request itself: what
    /// `landed` makes of what GitHub now shows, `Some` once the write is seen to have
    /// landed.
    async fn confirm_on_pull<A>(
        &self,
        target: &PrTarget,
        landed: impl FnOnce(&GithubPull) -> Option<A>,
    ) -> Result<Option<A>, GithubError> {
        let pull = self.fetch_target(target).await?;
        Ok(landed(&pull))
    }
}

/// What the mutate step of a verified mutation did.
enum Write<A> {
    /// The write went out; GitHub's raw answer.
    Sent(Response),
    /// An earlier attempt already did the work, so nothing was sent.
    AlreadyApplied(A),
}

/// How a verified mutation ended.
enum MutationOutcome<T, A> {
    /// GitHub answered and the client read the answer.
    Answered(T),
    /// The work is known to have happened without a readable answer: an earlier attempt
    /// did it, or a read-back confirmed it after an ambiguous answer.
    Applied(A),
}

impl<T, A> MutationOutcome<T, A> {
    /// What the mutation did: what was already known of it, or what `answered` reads
    /// out of GitHub's answer.
    fn into_applied(
        self,
        answered: impl FnOnce(T) -> Result<A, GithubError>,
    ) -> Result<A, GithubError> {
        match self {
            MutationOutcome::Answered(answer) => answered(answer),
            MutationOutcome::Applied(applied) => Ok(applied),
        }
    }
}

/// A merge that landed: the words for how the client knows — GitHub's answer, the pull
/// request found merged already, or a read-back after an ambiguous answer — and the
/// commit it made, as GitHub names it in the answer and on the merged pull request
/// alike. `None` when GitHub named none, which its schema allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Merged {
    pub detail: String,
    pub sha: Option<String>,
}

#[async_trait]
pub trait GithubApi: Send + Sync {
    fn installation_id(&self) -> u64;
    /// Whether a user identity is configured to post `@dependabot` commands under.
    /// Merge and update branch run as the App and need no such identity; a
    /// deployment without one has [`Self::post_command`] refuse every request.
    fn can_post_commands(&self) -> bool;
    async fn fetch_snapshot(&self, request: &SyncRequest) -> Result<Option<PrRecord>, GithubError>;
    async fn list_installation_repositories(&self) -> Result<Vec<RepoRecord>, GithubError>;
    async fn list_dependabot_prs(
        &self,
        owner: &str,
        repo: &str,
        repository_id: u64,
    ) -> Result<Vec<SyncRequest>, GithubError>;
    /// Merges the target with `merge_method`, the method its repository resolved
    /// at its last sync because it disallows the configured preference, or with
    /// the preference when `None`. Names the commit the merge made.
    async fn merge(
        &self,
        request: &MergeRequest,
        merge_method: Option<MergeMethod>,
    ) -> Result<Merged, GithubError>;
    async fn post_command(&self, request: &CommandRequest) -> Result<String, GithubError>;
    async fn update_branch(&self, request: &UpdateBranchRequest) -> Result<String, GithubError>;
}

#[async_trait]
impl GithubApi for GithubClient {
    fn installation_id(&self) -> u64 {
        self.installation_id()
    }

    fn can_post_commands(&self) -> bool {
        self.user_commands
    }

    /// One GraphQL request in the common case, where the REST reads it replaced cost at
    /// least five (pull request, head commit, check runs, check suites, combined status),
    /// more with pagination. Only a commit with over a hundred check contexts or suites
    /// costs a further request per extra page — and a pull request the dashboard does not
    /// project costs the one request that says so.
    async fn fetch_snapshot(&self, request: &SyncRequest) -> Result<Option<PrRecord>, GithubError> {
        let pull = self.fetch_pull_request_node(request).await?;
        if !pull.is_open_dependabot_pull() {
            return Ok(None);
        }
        let head = self.fetch_head_commit(request, &pull).await?;
        let signals = self.check_signals(request, &head).await?;
        project_snapshot(
            request,
            self.config.installation_id,
            &pull,
            &head,
            signals,
            unix_seconds(),
        )
        .map(Some)
    }

    /// Reads every page of the installation's repositories — until one is short, or the
    /// pages reach the total the first reported — and folds them, refusing a listing that
    /// shifted under its pages (see `fold_repository_pages`).
    async fn list_installation_repositories(&self) -> Result<Vec<RepoRecord>, GithubError> {
        let synced_at = unix_seconds();
        let mut pages: Vec<InstallationRepositories> = Vec::new();
        let mut listed = 0;
        loop {
            let page: InstallationRepositories = self
                .installation_json(
                    self.config.installation_id,
                    Method::GET,
                    &format!(
                        "/installation/repositories?per_page={LISTING_PAGE_SIZE}&page={}",
                        pages.len() + 1
                    ),
                    None,
                )
                .await?;
            listed += page.repositories.len();
            let total = pages
                .first()
                .map_or(page.total_count, |first| first.total_count);
            let last = page.repositories.len() < LISTING_PAGE_SIZE || listed >= total;
            pages.push(page);
            if last {
                break;
            }
        }
        fold_repository_pages(
            pages,
            self.config.installation_id,
            self.config.merge_method,
            synced_at,
        )
    }

    async fn list_dependabot_prs(
        &self,
        owner: &str,
        repo: &str,
        repository_id: u64,
    ) -> Result<Vec<SyncRequest>, GithubError> {
        let mut page = 1;
        let mut pulls = Vec::new();
        loop {
            let response: Vec<PullListItem> = self
                .installation_json(
                    self.config.installation_id,
                    Method::GET,
                    &format!(
                        "/repos/{owner}/{repo}/pulls?state=open&per_page={LISTING_PAGE_SIZE}&page={page}"
                    ),
                    None,
                )
                .await?;
            let count = response.len();
            pulls.extend(
                response
                    .into_iter()
                    .filter(|pull| pull.user.login == DEPENDABOT_LOGIN)
                    .map(|pull| SyncRequest {
                        repository_id,
                        owner: owner.to_owned(),
                        repo: repo.to_owned(),
                        number: pull.number,
                        bypass_debounce: false,
                        completion_id: None,
                    }),
            );
            if count < LISTING_PAGE_SIZE {
                break;
            }
            page += 1;
        }
        Ok(pulls)
    }

    async fn merge(
        &self,
        request: &MergeRequest,
        merge_method: Option<MergeMethod>,
    ) -> Result<Merged, GithubError> {
        let target = &request.target;
        let path = format!(
            "/repos/{}/{}/pulls/{}/merge",
            target.owner, target.repo, target.number
        );
        let body = json!({
            "sha": target.expected_sha,
            "merge_method": merge_method
                .unwrap_or(self.config.merge_method)
                .to_string(),
        });
        self.verified_mutation(
            Operation::Merge,
            target,
            merged_as("already merged"),
            async || {
                self.installation_request(
                    self.config.installation_id,
                    Method::PUT,
                    &path,
                    Some(&body),
                )
                .await
                .map(Write::Sent)
            },
            async || {
                self.confirm_on_pull(
                    target,
                    merged_as("merged (confirmed after an ambiguous response)"),
                )
                .await
            },
        )
        .await?
        .into_applied(|answer: MergeResult| {
            if answer.merged {
                Ok(Merged {
                    detail: answer.message,
                    sha: answer.sha,
                })
            } else {
                Err(GithubError::Http {
                    response: GithubErrorResponse {
                        status: StatusCode::CONFLICT.as_u16(),
                        message: answer.message,
                        ..Default::default()
                    },
                    known_resource: true,
                })
            }
        })
    }

    /// Refused outright without a user identity: the verify read would only be wasted
    /// on a comment that can never be posted.
    async fn post_command(&self, request: &CommandRequest) -> Result<String, GithubError> {
        if !self.user_commands {
            return Err(no_user_token());
        }
        let target = &request.target;
        let marker = format!("<!-- dependaboard-batch:{} -->", request.batch_id);
        let path = format!(
            "/repos/{}/{}/issues/{}/comments",
            target.owner, target.repo, target.number
        );
        let body = json!({
            "body": format!(
                "@dependabot {}\n\n— via dependabot-dashboard ({})\n{}",
                request.command, request.user_id, marker
            )
        });
        self.verified_mutation(
            Operation::Comment,
            target,
            |_| None,
            async || {
                if let Some(comment_id) = self.find_comment_with_marker(&path, &marker).await? {
                    return Ok(Write::AlreadyApplied(format!(
                        "command already accepted as comment #{comment_id}"
                    )));
                }
                self.user_request(&request.user_id, Method::POST, &path, Some(&body))
                    .await
                    .map(Write::Sent)
            },
            async || {
                Ok(self
                    .find_comment_with_marker(&path, &marker)
                    .await?
                    .map(|comment_id| {
                        format!(
                            "command accepted as comment #{comment_id} (confirmed after an ambiguous response)"
                        )
                    }))
            },
        )
        .await?
        .into_applied(|comment: IssueComment| Ok(format!("GitHub accepted comment #{}", comment.id)))
    }

    async fn update_branch(&self, request: &UpdateBranchRequest) -> Result<String, GithubError> {
        let target = &request.target;
        let path = format!(
            "/repos/{}/{}/pulls/{}/update-branch",
            target.owner, target.repo, target.number
        );
        let body = json!({ "expected_head_sha": target.expected_sha });
        self.verified_mutation(
            Operation::UpdateBranch,
            target,
            |_| None,
            async || {
                self.installation_request(
                    self.config.installation_id,
                    Method::PUT,
                    &path,
                    Some(&body),
                )
                .await
                .map(Write::Sent)
            },
            async || {
                self.confirm_on_pull(target, |pull| {
                    (pull.head.sha != target.expected_sha).then(|| {
                        "branch updated (confirmed after an ambiguous response)".to_owned()
                    })
                })
                .await
            },
        )
        .await?
        .into_applied(|answer: UpdateBranchResult| Ok(answer.message))
    }
}

/// What `signal` makes of every node of a paged connection: those of `first`, the page
/// that came with the commit, then of each page `next_page` reads for the cursor the
/// page before it ended on.
async fn signals_across_pages<T, Fut>(
    first: &Connection<T>,
    signal: impl Fn(&T) -> Option<CheckSignal>,
    mut next_page: impl FnMut(String) -> Fut,
) -> Result<Vec<CheckSignal>, GithubError>
where
    Fut: Future<Output = Result<Option<Connection<T>>, GithubError>>,
{
    let mut signals: Vec<CheckSignal> = first.nodes.iter().filter_map(&signal).collect();
    let mut cursor = first.next_cursor().map(str::to_owned);
    while let Some(after) = cursor.take() {
        if let Some(page) = next_page(after).await? {
            signals.extend(page.nodes.iter().filter_map(&signal));
            cursor = page.next_cursor().map(str::to_owned);
        }
    }
    Ok(signals)
}

/// A merge seen on the pull request rather than in GitHub's answer to the merge — found
/// done before anything was sent, or confirmed after an ambiguous answer — with `detail`
/// as the words for it. It names the commit the pull request does.
fn merged_as(detail: &'static str) -> impl FnOnce(&GithubPull) -> Option<Merged> {
    move |pull| {
        pull.merged.then(|| Merged {
            detail: detail.to_owned(),
            sha: pull.merge_commit_sha.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway RSA key generated for the test suite.
    const APP_KEY: &str = include_str!("../testdata/app-key.pem");

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    fn required_env(private_key: &str) -> Vec<(&str, &str)> {
        vec![
            ("GITHUB_APP_ID", "1234"),
            ("GITHUB_INSTALLATION_ID", "42"),
            ("GITHUB_PRIVATE_KEY", private_key),
            ("GITHUB_USER_PAT", "ghp_pat"),
        ]
    }

    #[test]
    fn multiline_private_key_env_is_normalized_into_a_usable_key() {
        // Secret stores commonly flatten PEM files to one line with literal `\n`.
        let flattened = APP_KEY.trim_end().replace('\n', "\\n");

        let config = GithubConfig::from_lookup(lookup(&required_env(&flattened))).unwrap();

        assert_eq!(config.private_key.expose_secret(), APP_KEY.trim_end());
        assert!(GithubClient::new(config).is_ok());
    }

    /// A malformed key is refused when the client is built, before any API call.
    #[test]
    fn an_invalid_private_key_fails_client_construction() {
        let config = GithubConfig::from_lookup(lookup(&required_env(
            "-----BEGIN RSA PRIVATE KEY-----\nnot a key\n",
        )))
        .expect("the key is not read until the client is built");

        let error = GithubClient::new(config).err().expect("construction fails");

        assert!(
            matches!(&error, GithubError::Config(message) if message.contains("private key")),
            "expected a private-key config error, got {error:?}"
        );
    }

    #[test]
    fn config_applies_defaults_and_honours_overrides() {
        let mut vars = required_env(APP_KEY);
        let defaults = GithubConfig::from_lookup(lookup(&vars)).unwrap();
        assert_eq!(defaults.api_url, "https://api.github.com");
        assert_eq!(defaults.app_id, 1234);
        assert_eq!(defaults.installation_id, 42);
        assert_eq!(defaults.dashboard_user, UserId::new("dependaboard"));
        assert_eq!(defaults.merge_method, MergeMethod::Squash);

        vars.push(("GITHUB_API_URL", "https://ghe.example.com/api/v3/"));
        vars.push(("GITHUB_MERGE_METHOD", "rebase"));
        vars.push(("DASHBOARD_USERNAME", "octocat"));
        let custom = GithubConfig::from_lookup(lookup(&vars)).unwrap();
        assert_eq!(custom.api_url, "https://ghe.example.com/api/v3");
        assert_eq!(custom.merge_method, MergeMethod::Rebase);
        assert_eq!(custom.dashboard_user, UserId::new("octocat"));
    }

    /// Only `@dependabot` commands need the user PAT; merge and update branch run as
    /// the App. A merge-only operator runs without one, and a placeholder left blank
    /// counts as none.
    #[test]
    fn the_user_pat_is_optional() {
        let with_pat = GithubConfig::from_lookup(lookup(&required_env(APP_KEY))).unwrap();
        assert_eq!(
            with_pat.user_pat.as_ref().map(ExposeSecret::expose_secret),
            Some("ghp_pat")
        );

        let mut vars = required_env(APP_KEY);
        vars.retain(|(name, _)| *name != "GITHUB_USER_PAT");
        let without = GithubConfig::from_lookup(lookup(&vars)).unwrap();
        assert!(without.user_pat.is_none());

        vars.push(("GITHUB_USER_PAT", ""));
        let blank = GithubConfig::from_lookup(lookup(&vars)).unwrap();
        assert!(blank.user_pat.is_none(), "a blank value is no PAT");
    }

    #[test]
    fn config_rejects_missing_or_malformed_settings() {
        let missing_key = GithubConfig::from_lookup(lookup(&[("GITHUB_APP_ID", "1")]));
        assert!(matches!(
            missing_key,
            Err(GithubError::Config(message)) if message.contains("GITHUB_PRIVATE_KEY")
        ));

        let mut vars = required_env(APP_KEY);
        vars.push(("GITHUB_MERGE_METHOD", "fast-forward"));
        assert!(matches!(
            GithubConfig::from_lookup(lookup(&vars)),
            Err(GithubError::Config(message)) if message.contains("GITHUB_MERGE_METHOD")
        ));

        let mut vars = required_env(APP_KEY);
        vars[0] = ("GITHUB_APP_ID", "not-a-number");
        assert!(matches!(
            GithubConfig::from_lookup(lookup(&vars)),
            Err(GithubError::Config(message)) if message.contains("GITHUB_APP_ID")
        ));
    }
}
