use std::{collections::HashMap, env, fs, sync::Arc, time::Duration};

use async_trait::async_trait;
use dependaboard_core::{
    CheckSignal, CommandRequest, DEPENDABOT_LOGIN, GithubErrorResponse, MergeMethod, MergeRequest,
    Mergeable, PrKey, PrRecord, RepoRecord, SyncRequest, UpdateBranchRequest, UserId,
    combined_status_signal, highest_update_type, parse_dependabot_metadata, rollup_checks,
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
        let private_key = match env::var("GITHUB_PRIVATE_KEY") {
            Ok(value) => value.replace("\\n", "\n"),
            Err(_) => {
                let path = env::var("GITHUB_PRIVATE_KEY_PATH").map_err(|_| {
                    GithubError::Config(
                        "set GITHUB_PRIVATE_KEY or GITHUB_PRIVATE_KEY_PATH".to_owned(),
                    )
                })?;
                fs::read_to_string(path).map_err(|error| {
                    GithubError::Config(format!("cannot read private key: {error}"))
                })?
            }
        };
        let dashboard_user =
            env::var("DASHBOARD_USERNAME").unwrap_or_else(|_| "dependaboard".to_owned());
        if dashboard_user.trim().is_empty() {
            return Err(GithubError::Config(
                "DASHBOARD_USERNAME must not be empty".to_owned(),
            ));
        }
        let merge_method = env::var("GITHUB_MERGE_METHOD")
            .unwrap_or_else(|_| MergeMethod::default().to_string())
            .parse()
            .map_err(|_| {
                GithubError::Config(
                    "GITHUB_MERGE_METHOD must be merge, squash, or rebase".to_owned(),
                )
            })?;
        Ok(Self {
            api_url: env::var("GITHUB_API_URL")
                .unwrap_or_else(|_| "https://api.github.com".to_owned())
                .trim_end_matches('/')
                .to_owned(),
            app_id: parse_env("GITHUB_APP_ID")?,
            installation_id: parse_env("GITHUB_INSTALLATION_ID")?,
            private_key: SecretString::from(private_key),
            user_pat: SecretString::from(env::var("GITHUB_USER_PAT").map_err(|_| {
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

fn parse_env(name: &str) -> Result<u64, GithubError> {
    env::var(name)
        .map_err(|_| GithubError::Config(format!("set {name}")))?
        .parse()
        .map_err(|_| GithubError::Config(format!("{name} must be an unsigned integer")))
}

#[derive(Clone)]
pub struct GithubClient {
    config: Arc<GithubConfig>,
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
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| GithubError::Transport(error.to_string()))?;
        Ok(Self {
            config: Arc::new(config),
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
            .map_err(|error| GithubError::Transport(error.to_string()))?;
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
        let key = EncodingKey::from_rsa_pem(self.config.private_key.expose_secret().as_bytes())
            .map_err(|error| GithubError::Config(format!("invalid GitHub private key: {error}")))?;
        encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                iat: now.saturating_sub(60),
                exp: now + 9 * 60,
                iss: self.config.app_id.to_string(),
            },
            &key,
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
            let response = request
                .send()
                .await
                .map_err(|error| GithubError::Transport(error.to_string()))?;
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
        request
            .send()
            .await
            .map_err(|error| GithubError::Transport(error.to_string()))
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
        let pull = self
            .fetch_pull(
                self.config.installation_id,
                &request.target.owner,
                &request.target.repo,
                request.target.number,
            )
            .await?;
        if pull.merged {
            return Ok("already merged".to_owned());
        }
        if pull.state != "open" || pull.user.login != DEPENDABOT_LOGIN {
            return Err(not_found(
                "pull request is closed or is not owned by Dependabot",
            ));
        }
        if pull.head.sha != request.target.expected_sha {
            return Err(GithubError::StaleSha {
                expected: request.target.expected_sha.clone(),
                actual: pull.head.sha,
            });
        }
        let path = format!(
            "/repos/{}/{}/pulls/{}/merge",
            request.target.owner, request.target.repo, request.target.number
        );
        let body = json!({
            "sha": request.target.expected_sha,
            "merge_method": self.config.merge_method.to_string(),
        });
        let response = match self
            .installation_request(self.config.installation_id, Method::PUT, &path, Some(&body))
            .await
        {
            Ok(response) => response,
            Err(original) => {
                if self
                    .fetch_pull(
                        self.config.installation_id,
                        &request.target.owner,
                        &request.target.repo,
                        request.target.number,
                    )
                    .await
                    .is_ok_and(|pull| pull.merged)
                {
                    return Ok("merged (confirmed after an ambiguous response)".to_owned());
                }
                return Err(original);
            }
        };
        let result: MergeResult = match parse_response(response).await.map_err(known_http) {
            Ok(result) => result,
            Err(GithubError::Protocol(message)) => {
                if self
                    .fetch_pull(
                        self.config.installation_id,
                        &request.target.owner,
                        &request.target.repo,
                        request.target.number,
                    )
                    .await
                    .is_ok_and(|pull| pull.merged)
                {
                    return Ok("merged (confirmed after an ambiguous response)".to_owned());
                }
                return Err(GithubError::Transport(format!(
                    "ambiguous merge response: {message}"
                )));
            }
            Err(error) => return Err(error),
        };
        if result.merged {
            Ok(result.message)
        } else {
            Err(GithubError::HttpKnown(GithubErrorResponse {
                status: StatusCode::CONFLICT.as_u16(),
                message: result.message,
                ..Default::default()
            }))
        }
    }

    async fn post_command(&self, request: &CommandRequest) -> Result<String, GithubError> {
        let pull = self
            .fetch_pull(
                self.config.installation_id,
                &request.target.owner,
                &request.target.repo,
                request.target.number,
            )
            .await?;
        if pull.state != "open" || pull.user.login != DEPENDABOT_LOGIN {
            return Err(not_found(
                "pull request is closed or is not owned by Dependabot",
            ));
        }
        if pull.head.sha != request.target.expected_sha {
            return Err(GithubError::StaleSha {
                expected: request.target.expected_sha.clone(),
                actual: pull.head.sha,
            });
        }

        let marker = format!("<!-- dependaboard-batch:{} -->", request.batch_id);
        let path = format!(
            "/repos/{}/{}/issues/{}/comments",
            request.target.owner, request.target.repo, request.target.number
        );
        if let Some(comment_id) = self.find_comment_with_marker(&path, &marker).await? {
            return Ok(format!("command already accepted as comment #{comment_id}"));
        }
        let body = json!({
            "body": format!(
                "@dependabot {}\n\n— via dependabot-dashboard ({})\n{}",
                request.command, request.user_id, marker
            )
        });
        let response = match self
            .user_request(&request.user_id, Method::POST, &path, Some(&body))
            .await
        {
            Ok(response) => response,
            Err(original) => {
                if let Some(comment_id) = self.find_comment_with_marker(&path, &marker).await? {
                    return Ok(format!(
                        "command accepted as comment #{comment_id} (confirmed after an ambiguous response)"
                    ));
                }
                return Err(original);
            }
        };
        let comment: IssueComment = match parse_response(response).await.map_err(known_http) {
            Ok(comment) => comment,
            Err(GithubError::Protocol(message)) => {
                if let Some(comment_id) = self.find_comment_with_marker(&path, &marker).await? {
                    return Ok(format!(
                        "command accepted as comment #{comment_id} (confirmed after an ambiguous response)"
                    ));
                }
                return Err(GithubError::Transport(format!(
                    "ambiguous comment response: {message}"
                )));
            }
            Err(error) => return Err(error),
        };
        Ok(format!("GitHub accepted comment #{}", comment.id))
    }

    async fn update_branch(&self, request: &UpdateBranchRequest) -> Result<String, GithubError> {
        let pull = self
            .fetch_pull(
                self.config.installation_id,
                &request.target.owner,
                &request.target.repo,
                request.target.number,
            )
            .await?;
        if pull.state != "open" || pull.user.login != DEPENDABOT_LOGIN {
            return Err(not_found(
                "pull request is closed or is not owned by Dependabot",
            ));
        }
        if pull.head.sha != request.target.expected_sha {
            return Err(GithubError::StaleSha {
                expected: request.target.expected_sha.clone(),
                actual: pull.head.sha,
            });
        }
        let path = format!(
            "/repos/{}/{}/pulls/{}/update-branch",
            request.target.owner, request.target.repo, request.target.number
        );
        let body = json!({ "expected_head_sha": request.target.expected_sha });
        let response = match self
            .installation_request(self.config.installation_id, Method::PUT, &path, Some(&body))
            .await
        {
            Ok(response) => response,
            Err(original) => {
                if self
                    .fetch_pull(
                        self.config.installation_id,
                        &request.target.owner,
                        &request.target.repo,
                        request.target.number,
                    )
                    .await
                    .is_ok_and(|pull| pull.head.sha != request.target.expected_sha)
                {
                    return Ok("branch updated (confirmed after an ambiguous response)".to_owned());
                }
                return Err(original);
            }
        };
        let result: UpdateBranchResult = match parse_response(response).await.map_err(known_http) {
            Ok(result) => result,
            Err(GithubError::Protocol(message)) => {
                if self
                    .fetch_pull(
                        self.config.installation_id,
                        &request.target.owner,
                        &request.target.repo,
                        request.target.number,
                    )
                    .await
                    .is_ok_and(|pull| pull.head.sha != request.target.expected_sha)
                {
                    return Ok("branch updated (confirmed after an ambiguous response)".to_owned());
                }
                return Err(GithubError::Transport(format!(
                    "ambiguous update-branch response: {message}"
                )));
            }
            Err(error) => return Err(error),
        };
        Ok(result.message)
    }
}

fn not_found(message: &str) -> GithubError {
    GithubError::HttpKnown(GithubErrorResponse {
        status: StatusCode::NOT_FOUND.as_u16(),
        message: message.to_owned(),
        ..Default::default()
    })
}

fn known_http(error: GithubError) -> GithubError {
    match error {
        GithubError::Http(response) => GithubError::HttpKnown(response),
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
            .map_err(|error| GithubError::Protocol(format!("invalid GitHub response: {error}")));
    }
    Err(GithubError::Http(http_error(response).await))
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
        .map_err(|error| GithubError::Protocol(format!("invalid GitHub timestamp: {error}")))?
        .timestamp();
    u64::try_from(timestamp)
        .map_err(|_| GithubError::Protocol("GitHub returned a timestamp before 1970".to_owned()))
}

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("GitHub transport failed: {0}")]
    Transport(String),
    #[error("GitHub returned HTTP {status}: {message}", status = .0.status, message = .0.message)]
    Http(GithubErrorResponse),
    #[error("GitHub returned HTTP {status}: {message}", status = .0.status, message = .0.message)]
    HttpKnown(GithubErrorResponse),
    #[error("GitHub protocol error: {0}")]
    Protocol(String),
    #[error("GitHub configuration error: {0}")]
    Config(String),
    #[error("pull request head changed from {expected} to {actual}")]
    StaleSha { expected: String, actual: String },
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

    #[test]
    fn multiline_private_key_env_is_normalized() {
        assert_eq!("a\\nb".replace("\\n", "\n"), "a\nb");
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
