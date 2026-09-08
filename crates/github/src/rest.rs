//! The wire shapes of GitHub's REST answers, kept to the fields the client
//! looks at, and how any answer — GraphQL's included — is read: its status,
//! the rate-limit headers GitHub puts on every one, its timestamps, its body.
//! The requests that earn the answers are the client's own, in `lib.rs`.

use dependaboard_core::{GithubErrorResponse, MergeMethod};
use reqwest::Response;
use serde::{Deserialize, de::DeserializeOwned};

use crate::{GithubError, ProtocolError};

pub(crate) async fn parse_response<T: DeserializeOwned>(
    response: Response,
) -> Result<T, GithubError> {
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

pub(crate) async fn http_error(response: Response) -> GithubErrorResponse {
    let status = response.status().as_u16();
    let rate_limit = RateLimitHeaders::read(&response);
    let body = response.json::<GithubErrorBody>().await.unwrap_or_default();
    rate_limit.error(status, body.message, body.documentation_url)
}

/// The rate-limit headers GitHub attaches to every answer, REST or GraphQL, as unix
/// seconds and counts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RateLimitHeaders {
    pub(crate) remaining: Option<u64>,
    pub(crate) reset: Option<u64>,
    pub(crate) retry_after: Option<u64>,
}

impl RateLimitHeaders {
    pub(crate) fn read(response: &Response) -> Self {
        Self {
            remaining: header_u64(response, "x-ratelimit-remaining"),
            reset: header_u64(response, "x-ratelimit-reset"),
            retry_after: header_u64(response, "retry-after"),
        }
    }

    /// A failed answer that carried these headers.
    pub(crate) fn error(
        self,
        status: u16,
        message: String,
        documentation_url: Option<String>,
    ) -> GithubErrorResponse {
        GithubErrorResponse {
            status,
            message,
            documentation_url,
            rate_limit_remaining: self.remaining,
            rate_limit_reset: self.reset,
            retry_after_seconds: self.retry_after,
        }
    }
}

fn header_u64(response: &Response, name: &str) -> Option<u64> {
    response.headers().get(name)?.to_str().ok()?.parse().ok()
}

pub(crate) fn parse_timestamp(value: &str) -> Result<u64, GithubError> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|error| GithubError::Protocol(ProtocolError::Timestamp(error)))?
        .timestamp();
    u64::try_from(timestamp).map_err(|_| GithubError::Protocol(ProtocolError::TimestampBeforeEpoch))
}

#[derive(Debug, Default, Deserialize)]
struct GithubErrorBody {
    #[serde(default)]
    message: String,
    documentation_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubUser {
    pub(crate) login: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubHead {
    pub(crate) sha: String,
}

/// The REST view of a pull request, read to verify a mutation's target: snapshots come
/// from GraphQL, so only the fields the verify and confirm steps look at are kept. The
/// merge commit is one: a merge found already done, or confirmed after an ambiguous
/// answer, has no answer of GitHub's to read it from.
#[derive(Debug, Deserialize)]
pub(crate) struct GithubPull {
    pub(crate) state: String,
    pub(crate) user: GithubUser,
    pub(crate) head: GithubHead,
    #[serde(default)]
    pub(crate) merged: bool,
    #[serde(default)]
    pub(crate) merge_commit_sha: Option<String>,
}

/// One page of `GET /installation/repositories`: the page's repositories beside how many
/// the installation has in all, which is what lets a listing notice it moved under its
/// own pages.
#[derive(Debug, Deserialize)]
pub(crate) struct InstallationRepositories {
    pub(crate) total_count: usize,
    pub(crate) repositories: Vec<GithubRepository>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubRepository {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) owner: GithubUser,
    #[serde(flatten)]
    pub(crate) merge_settings: MergeSettings,
}

/// Which merge methods a repository's settings permit. GitHub's schema marks
/// each flag optional with a default of `true`, so an absent flag reads as
/// allowed.
#[derive(Debug, Deserialize)]
pub(crate) struct MergeSettings {
    #[serde(default = "allowed_by_default")]
    allow_squash_merge: bool,
    #[serde(default = "allowed_by_default")]
    allow_merge_commit: bool,
    #[serde(default = "allowed_by_default")]
    allow_rebase_merge: bool,
}

fn allowed_by_default() -> bool {
    true
}

impl MergeSettings {
    fn allows(&self, method: MergeMethod) -> bool {
        match method {
            MergeMethod::Squash => self.allow_squash_merge,
            MergeMethod::Merge => self.allow_merge_commit,
            MergeMethod::Rebase => self.allow_rebase_merge,
        }
    }

    /// The method to merge with instead of `preferred`, when these settings
    /// disallow it: the first allowed of squash, merge, rebase. `None` when
    /// `preferred` is allowed, or when nothing is (GitHub then decides).
    pub(crate) fn method_instead_of(&self, preferred: MergeMethod) -> Option<MergeMethod> {
        if self.allows(preferred) {
            return None;
        }
        [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase]
            .into_iter()
            .find(|method| self.allows(*method))
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PullListItem {
    pub(crate) number: u64,
    pub(crate) user: GithubUser,
}

/// GitHub's answer to `PUT /pulls/{n}/merge`: whether it merged, in words, and as what
/// commit.
#[derive(Debug, Deserialize)]
pub(crate) struct MergeResult {
    pub(crate) merged: bool,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) sha: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateBranchResult {
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct IssueComment {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) body: String,
}
