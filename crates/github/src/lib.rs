use std::{collections::HashMap, env, fs, sync::Arc, time::Duration};

use async_trait::async_trait;
use dependaboard_core::{
    CheckSignal, CommandRequest, DEPENDABOT_LOGIN, GithubErrorResponse, MergeMethod, MergeRequest,
    Mergeable, Operation, PrKey, PrRecord, PrTarget, RepoRecord, SyncRequest, UpdateBranchRequest,
    UserId, combined_status_signal, highest_update_type, parse_dependabot_metadata, rollup_checks,
    unix_seconds,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::{Method, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex;

const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "dependaboard/0.1";

#[derive(Clone)]
pub struct GithubConfig {
    pub api_url: String,
    pub app_id: u64,
    pub installation_id: u64,
    pub private_key: SecretString,
    pub user_pat: SecretString,
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
            user_pat: SecretString::from(var("GITHUB_USER_PAT").ok_or_else(|| {
                GithubError::Config("set GITHUB_USER_PAT for Dependabot rebase commands".to_owned())
            })?),
            dashboard_user: UserId::new(dashboard_user),
            merge_method,
        })
    }
}

#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn user_token(&self, user: &UserId) -> Result<SecretString, GithubError>;
}

struct StaticTokenProvider {
    user: UserId,
    token: SecretString,
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn user_token(&self, user: &UserId) -> Result<SecretString, GithubError> {
        if user == &self.user {
            Ok(self.token.clone())
        } else {
            Err(GithubError::Config(format!(
                "no GitHub user token is configured for {user}"
            )))
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
}

#[derive(Clone)]
struct CachedToken {
    value: SecretString,
    expires_at: u64,
}

impl GithubClient {
    pub fn new(config: GithubConfig) -> Result<Self, GithubError> {
        let user_tokens = Arc::new(StaticTokenProvider {
            user: config.dashboard_user.clone(),
            token: config.user_pat.clone(),
        });
        Self::with_token_provider(config, user_tokens)
    }

    pub fn with_token_provider(
        config: GithubConfig,
        user_tokens: Arc<dyn TokenProvider>,
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
        })
    }

    pub fn installation_id(&self) -> u64 {
        self.config.installation_id
    }

    async fn installation_token(
        &self,
        installation_id: u64,
        force_refresh: bool,
    ) -> Result<SecretString, GithubError> {
        let now = unix_seconds();
        let mut tokens = self.tokens.lock().await;
        if !force_refresh
            && let Some(token) = tokens.get(&installation_id)
            && token.expires_at > now + 60
        {
            return Ok(token.value.clone());
        }
        let jwt = self.app_jwt()?;
        let response = self
            .http
            .post(format!(
                "{}/app/installations/{installation_id}/access_tokens",
                self.config.api_url
            ))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(jwt)
            .send()
            .await
            .map_err(GithubError::Transport)?;
        let token: InstallationToken = parse_response(response).await?;
        let expires_at = parse_timestamp(&token.expires_at)?;
        let value = SecretString::from(token.token);
        tokens.insert(
            installation_id,
            CachedToken {
                value: value.clone(),
                expires_at,
            },
        );
        Ok(value)
    }

    fn app_jwt(&self) -> Result<String, GithubError> {
        #[derive(Serialize)]
        struct Claims {
            iat: u64,
            exp: u64,
            iss: String,
        }

        let now = unix_seconds();
        encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                iat: now.saturating_sub(60),
                exp: now + 9 * 60,
                iss: self.config.app_id.to_string(),
            },
            &self.app_key,
        )
        .map_err(|error| GithubError::Config(format!("cannot sign GitHub App JWT: {error}")))
    }

    async fn installation_request(
        &self,
        installation_id: u64,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Response, GithubError> {
        for attempt in 0..2 {
            let token = self
                .installation_token(installation_id, attempt > 0)
                .await?;
            let mut request = self
                .http
                .request(method.clone(), format!("{}{}", self.config.api_url, path))
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

    async fn fetch_commit_message(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<String, GithubError> {
        let response: GithubCommit = self
            .installation_json(
                installation_id,
                Method::GET,
                &format!("/repos/{owner}/{repo}/commits/{sha}"),
                None,
            )
            .await?;
        Ok(response.commit.message)
    }

    async fn fetch_check_signals(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Vec<CheckSignal>, GithubError> {
        let mut page = 1;
        let mut signals = Vec::new();
        loop {
            let response: CheckRuns = self
                .installation_json(
                    installation_id,
                    Method::GET,
                    &format!(
                        "/repos/{owner}/{repo}/commits/{sha}/check-runs?per_page=100&page={page}"
                    ),
                    None,
                )
                .await?;
            let count = response.check_runs.len();
            signals.extend(response.check_runs.into_iter().filter_map(|run| {
                dependaboard_core::check_signal(run.status.as_deref(), run.conclusion.as_deref())
            }));
            if count < 100 {
                break;
            }
            page += 1;
        }
        let mut page = 1;
        loop {
            let response: CheckSuites = self
                .installation_json(
                    installation_id,
                    Method::GET,
                    &format!(
                        "/repos/{owner}/{repo}/commits/{sha}/check-suites?per_page=100&page={page}"
                    ),
                    None,
                )
                .await?;
            let count = response.check_suites.len();
            signals.extend(
                response
                    .check_suites
                    .into_iter()
                    .filter_map(|suite| check_suite_signal(&suite)),
            );
            if count < 100 {
                break;
            }
            page += 1;
        }
        let combined: CombinedStatus = self
            .installation_json(
                installation_id,
                Method::GET,
                &format!("/repos/{owner}/{repo}/commits/{sha}/status"),
                None,
            )
            .await?;
        if let Some(signal) = combined_status_signal(&combined.state, combined.total_count) {
            signals.push(signal);
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
                    &format!("{path}?per_page=100&page={page}"),
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
            if comments.len() < 100 {
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
    /// it did, the mutation succeeds with that detail. Otherwise a transport failure stands
    /// and an unreadable answer becomes [`GithubError::Ambiguous`]. Any other failure to
    /// send means nothing reached GitHub and is returned as is. A read-back that itself
    /// fails counts as unconfirmed.
    ///
    /// HTTP errors GitHub returns after the pull request was verified are flagged as coming
    /// from a known resource.
    async fn verified_mutation<T: DeserializeOwned>(
        &self,
        operation: Operation,
        target: &PrTarget,
        already_applied: impl FnOnce(&GithubPull) -> Option<String>,
        mutate: impl AsyncFnOnce() -> Result<Write, GithubError>,
        confirm: impl AsyncFnOnce() -> Result<Option<String>, GithubError>,
    ) -> Result<MutationOutcome<T>, GithubError> {
        let pull = self.fetch_target(target).await?;
        if let Some(detail) = already_applied(&pull) {
            return Ok(MutationOutcome::Applied(detail));
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
            Ok(Write::AlreadyApplied(detail)) => return Ok(MutationOutcome::Applied(detail)),
            Err(lost @ GithubError::Transport(_)) => {
                return match confirm().await {
                    Ok(Some(detail)) => Ok(MutationOutcome::Applied(detail)),
                    Ok(None) | Err(_) => Err(lost),
                };
            }
            Err(error) => return Err(error),
        };
        match parse_response(response).await.map_err(known_http) {
            Ok(answer) => Ok(MutationOutcome::Answered(answer)),
            Err(GithubError::Protocol(unreadable)) => match confirm().await {
                Ok(Some(detail)) => Ok(MutationOutcome::Applied(detail)),
                Ok(None) | Err(_) => Err(GithubError::Ambiguous {
                    operation,
                    source: unreadable,
                }),
            },
            Err(error) => Err(error),
        }
    }

    /// The read-back for writes whose effect shows on the pull request itself: `Some(detail)`
    /// when `landed` holds for what GitHub now shows.
    async fn confirm_on_pull(
        &self,
        target: &PrTarget,
        detail: &str,
        landed: impl FnOnce(&GithubPull) -> bool,
    ) -> Result<Option<String>, GithubError> {
        let pull = self.fetch_target(target).await?;
        Ok(landed(&pull).then(|| detail.to_owned()))
    }
}

/// What the mutate step of a verified mutation did.
enum Write {
    /// The write went out; GitHub's raw answer.
    Sent(Response),
    /// An earlier attempt already did the work, so nothing was sent.
    AlreadyApplied(String),
}

/// How a verified mutation ended.
enum MutationOutcome<T> {
    /// GitHub answered and the client read the answer.
    Answered(T),
    /// The work is known to have happened without a readable answer: an earlier attempt
    /// did it, or a read-back confirmed it after an ambiguous answer.
    Applied(String),
}

impl<T> MutationOutcome<T> {
    /// The outcome's detail: the one already known, or the one `answered` reads out of
    /// GitHub's answer.
    fn into_detail(
        self,
        answered: impl FnOnce(T) -> Result<String, GithubError>,
    ) -> Result<String, GithubError> {
        match self {
            MutationOutcome::Answered(answer) => answered(answer),
            MutationOutcome::Applied(detail) => Ok(detail),
        }
    }
}

#[async_trait]
pub trait GithubApi: Send + Sync {
    fn installation_id(&self) -> u64;
    async fn fetch_snapshot(&self, request: &SyncRequest) -> Result<Option<PrRecord>, GithubError>;
    async fn list_installation_repositories(&self) -> Result<Vec<RepoRecord>, GithubError>;
    async fn list_dependabot_prs(
        &self,
        owner: &str,
        repo: &str,
        repository_id: u64,
    ) -> Result<Vec<SyncRequest>, GithubError>;
    async fn merge(&self, request: &MergeRequest) -> Result<String, GithubError>;
    async fn post_command(&self, request: &CommandRequest) -> Result<String, GithubError>;
    async fn update_branch(&self, request: &UpdateBranchRequest) -> Result<String, GithubError>;
}

#[async_trait]
impl GithubApi for GithubClient {
    fn installation_id(&self) -> u64 {
        self.installation_id()
    }

    async fn fetch_snapshot(&self, request: &SyncRequest) -> Result<Option<PrRecord>, GithubError> {
        let installation_id = self.config.installation_id;
        let pull = self
            .fetch_pull(
                installation_id,
                &request.owner,
                &request.repo,
                request.number,
            )
            .await?;
        if pull.state != "open" || pull.user.login != DEPENDABOT_LOGIN {
            return Ok(None);
        }
        let message = self
            .fetch_commit_message(
                installation_id,
                &request.owner,
                &request.repo,
                &pull.head.sha,
            )
            .await?;
        let dependencies = parse_dependabot_metadata(&message, &pull.title);
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
        let check_status = rollup_checks(
            self.fetch_check_signals(
                installation_id,
                &request.owner,
                &request.repo,
                &pull.head.sha,
            )
            .await?,
        );
        let synced_at = unix_seconds();
        Ok(Some(PrRecord {
            id: PrKey::new(request.repository_id, request.number).to_string(),
            repository_id: request.repository_id,
            installation_id,
            owner: request.owner.clone(),
            repo: request.repo.clone(),
            number: request.number,
            title: pull.title,
            html_url: pull.html_url,
            dependency,
            from_version,
            to_version,
            update_type: highest_update_type(&dependencies),
            dependencies,
            head_sha: pull.head.sha,
            check_status,
            mergeable: pull
                .mergeable_state
                .as_deref()
                .map_or(Mergeable::Unknown, Mergeable::from_github_state),
            labels: pull.labels.into_iter().map(|label| label.name).collect(),
            created_at: parse_timestamp(&pull.created_at)?,
            updated_at: parse_timestamp(&pull.updated_at)?,
            synced_at,
        }))
    }

    async fn list_installation_repositories(&self) -> Result<Vec<RepoRecord>, GithubError> {
        let mut page = 1;
        let mut repositories = Vec::new();
        let synced_at = unix_seconds();
        loop {
            let response: InstallationRepositories = self
                .installation_json(
                    self.config.installation_id,
                    Method::GET,
                    &format!("/installation/repositories?per_page=100&page={page}"),
                    None,
                )
                .await?;
            let count = response.repositories.len();
            repositories.extend(response.repositories.into_iter().map(|repo| RepoRecord {
                repository_id: repo.id,
                installation_id: self.config.installation_id,
                owner: repo.owner.login,
                repo: repo.name,
                synced_at,
            }));
            if count < 100 {
                break;
            }
            page += 1;
        }
        Ok(repositories)
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
                    &format!("/repos/{owner}/{repo}/pulls?state=open&per_page=100&page={page}"),
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
            if count < 100 {
                break;
            }
            page += 1;
        }
        Ok(pulls)
    }

    async fn merge(&self, request: &MergeRequest) -> Result<String, GithubError> {
        let target = &request.target;
        let path = format!(
            "/repos/{}/{}/pulls/{}/merge",
            target.owner, target.repo, target.number
        );
        let body = json!({
            "sha": target.expected_sha,
            "merge_method": self.config.merge_method.to_string(),
        });
        self.verified_mutation(
            Operation::Merge,
            target,
            |pull| pull.merged.then(|| "already merged".to_owned()),
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
                    "merged (confirmed after an ambiguous response)",
                    |pull| pull.merged,
                )
                .await
            },
        )
        .await?
        .into_detail(|answer: MergeResult| {
            if answer.merged {
                Ok(answer.message)
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

    async fn post_command(&self, request: &CommandRequest) -> Result<String, GithubError> {
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
        .into_detail(|comment: IssueComment| Ok(format!("GitHub accepted comment #{}", comment.id)))
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
                self.confirm_on_pull(
                    target,
                    "branch updated (confirmed after an ambiguous response)",
                    |pull| pull.head.sha != target.expected_sha,
                )
                .await
            },
        )
        .await?
        .into_detail(|answer: UpdateBranchResult| Ok(answer.message))
    }
}

fn not_found(message: &str) -> GithubError {
    GithubError::Http {
        response: GithubErrorResponse {
            status: StatusCode::NOT_FOUND.as_u16(),
            message: message.to_owned(),
            ..Default::default()
        },
        known_resource: true,
    }
}

/// Marks an HTTP error as coming from a resource the client has already read, so a 404
/// downstream means the pull request went away rather than that it was never visible.
fn known_http(error: GithubError) -> GithubError {
    match error {
        GithubError::Http { response, .. } => GithubError::Http {
            response,
            known_resource: true,
        },
        error => error,
    }
}

fn check_suite_signal(suite: &CheckRun) -> Option<CheckSignal> {
    // Runs carry active/pass state; suites only add failures that can occur before a run exists.
    dependaboard_core::check_signal(suite.status.as_deref(), suite.conclusion.as_deref())
        .filter(|signal| matches!(signal, CheckSignal::Fail))
}

async fn parse_response<T: DeserializeOwned>(response: Response) -> Result<T, GithubError> {
    let status = response.status();
    if status.is_success() {
        return response
            .json::<T>()
            .await
            .map_err(|error| GithubError::Protocol(ProtocolError::Body(error)));
    }
    Err(GithubError::Http {
        response: http_error(response).await,
        known_resource: false,
    })
}

async fn http_error(response: Response) -> GithubErrorResponse {
    let status = response.status().as_u16();
    let remaining = header_u64(&response, "x-ratelimit-remaining");
    let reset = header_u64(&response, "x-ratelimit-reset");
    let retry_after = header_u64(&response, "retry-after");
    let body = response.json::<GithubErrorBody>().await.unwrap_or_default();
    GithubErrorResponse {
        status,
        message: body.message,
        documentation_url: body.documentation_url,
        rate_limit_remaining: remaining,
        rate_limit_reset: reset,
        retry_after_seconds: retry_after,
    }
}

fn header_u64(response: &Response, name: &str) -> Option<u64> {
    response.headers().get(name)?.to_str().ok()?.parse().ok()
}

fn parse_timestamp(value: &str) -> Result<u64, GithubError> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|error| GithubError::Protocol(ProtocolError::Timestamp(error)))?
        .timestamp();
    u64::try_from(timestamp).map_err(|_| GithubError::Protocol(ProtocolError::TimestampBeforeEpoch))
}

#[derive(Debug, Error)]
pub enum GithubError {
    /// The request never completed: the client could not be built, the connection failed,
    /// or the response was lost before it could be read.
    #[error("GitHub transport failed: {0}")]
    Transport(#[source] reqwest::Error),
    /// GitHub answered with a non-success status.
    ///
    /// `known_resource` records whether the client had already proven the target exists
    /// when this came back: a 404 on a pull request it just read means "gone", while a
    /// 404 on the first read may be a permissions misconfiguration.
    #[error("GitHub returned HTTP {}: {}", response.status, response.message)]
    Http {
        response: GithubErrorResponse,
        known_resource: bool,
    },
    #[error("GitHub protocol error: {0}")]
    Protocol(#[source] ProtocolError),
    /// A mutation's answer from GitHub could not be read and reading back did not show the
    /// mutation as applied, so whether it happened is unknown. Safe to retry: every attempt
    /// re-verifies the pull request before acting.
    #[error("ambiguous GitHub {operation} response: {source}")]
    Ambiguous {
        operation: Operation,
        source: ProtocolError,
    },
    #[error("GitHub configuration error: {0}")]
    Config(String),
    #[error("pull request head changed from {expected} to {actual}")]
    StaleSha { expected: String, actual: String },
}

/// Why a successful GitHub answer could not be read.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid GitHub response: {0}")]
    Body(#[source] reqwest::Error),
    #[error("invalid GitHub timestamp: {0}")]
    Timestamp(#[source] chrono::ParseError),
    #[error("GitHub returned a timestamp before 1970")]
    TimestampBeforeEpoch,
}

#[derive(Debug, Deserialize)]
struct InstallationToken {
    token: String,
    expires_at: String,
}

#[derive(Debug, Default, Deserialize)]
struct GithubErrorBody {
    #[serde(default)]
    message: String,
    documentation_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GithubHead {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GithubLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GithubPull {
    title: String,
    html_url: String,
    state: String,
    user: GithubUser,
    head: GithubHead,
    #[serde(default)]
    labels: Vec<GithubLabel>,
    created_at: String,
    updated_at: String,
    mergeable_state: Option<String>,
    #[serde(default)]
    merged: bool,
}

#[derive(Debug, Deserialize)]
struct GithubCommit {
    commit: CommitData,
}

#[derive(Debug, Deserialize)]
struct CommitData {
    message: String,
}

#[derive(Debug, Deserialize)]
struct CheckRuns {
    check_runs: Vec<CheckRun>,
}

#[derive(Debug, Deserialize)]
struct CheckRun {
    status: Option<String>,
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckSuites {
    check_suites: Vec<CheckRun>,
}

#[derive(Debug, Deserialize)]
struct CombinedStatus {
    state: String,
    total_count: u64,
}

#[derive(Debug, Deserialize)]
struct InstallationRepositories {
    repositories: Vec<GithubRepository>,
}

#[derive(Debug, Deserialize)]
struct GithubRepository {
    id: u64,
    name: String,
    owner: GithubUser,
}

#[derive(Debug, Deserialize)]
struct PullListItem {
    number: u64,
    user: GithubUser,
}

#[derive(Debug, Deserialize)]
struct MergeResult {
    merged: bool,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct UpdateBranchResult {
    message: String,
}

#[derive(Debug, Deserialize)]
struct IssueComment {
    id: u64,
    #[serde(default)]
    body: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use dependaboard_core::DependencyUpdate;

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

    #[test]
    fn grouped_update_scalar_columns_remain_empty() {
        let dependencies = [
            DependencyUpdate {
                name: "a".to_owned(),
                from_version: None,
                to_version: None,
                update_type: dependaboard_core::UpdateType::Patch,
            },
            DependencyUpdate {
                name: "b".to_owned(),
                from_version: None,
                to_version: None,
                update_type: dependaboard_core::UpdateType::Minor,
            },
        ];
        assert_eq!(
            highest_update_type(&dependencies),
            dependaboard_core::UpdateType::Minor
        );
    }

    #[test]
    fn queued_suite_container_does_not_override_successful_checks() {
        let queued_suite = CheckRun {
            status: Some("queued".to_owned()),
            conclusion: None,
        };
        let failed_suite = CheckRun {
            status: Some("completed".to_owned()),
            conclusion: Some("startup_failure".to_owned()),
        };
        let signals = [Some(CheckSignal::Pass), check_suite_signal(&queued_suite)]
            .into_iter()
            .flatten();

        assert_eq!(
            rollup_checks(signals),
            dependaboard_core::CheckStatus::Success
        );
        assert_eq!(check_suite_signal(&failed_suite), Some(CheckSignal::Fail));
    }

    #[tokio::test]
    async fn static_token_provider_is_scoped_to_the_configured_user() {
        let provider = StaticTokenProvider {
            user: UserId::new("dependaboard"),
            token: SecretString::from("secret"),
        };
        assert!(
            provider
                .user_token(&UserId::new("dependaboard"))
                .await
                .is_ok()
        );
        assert!(
            provider
                .user_token(&UserId::new("someone-else"))
                .await
                .is_err()
        );
    }
}
