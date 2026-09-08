//! The wire shapes of GitHub's REST answers, kept to the fields the client
//! looks at, and how any answer — GraphQL's included — is read: its status,
//! the rate-limit headers GitHub puts on every one, its timestamps, its body —
//! and how the pages of a repository listing fold into records. The requests
//! that earn the answers are the client's own, in `lib.rs`.

use std::collections::HashSet;

use dependaboard_core::{GithubErrorResponse, MergeMethod, RepoRecord};
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

/// Folds the pages of one repository listing into the records of `installation_id`, or
/// refuses the listing as [`GithubError::Shifted`] when it moved under its own pages.
///
/// Every page reports the installation's total as of that page. One that disagrees with
/// the first means a repository joined or left in between, and offset pagination cannot
/// say which item the boundary shift dropped. A boundary that shifted without moving the
/// total shows as a repository listed twice where another was never listed: the distinct
/// ids fall short of the total. Either way the set is not authoritative; a fresh listing
/// starts over from the first page.
///
/// A repository whose settings disallow `preferred`, the configured merge method, records
/// the method to merge with instead (see [`MergeSettings::method_instead_of`]).
pub(crate) fn fold_repository_pages(
    pages: impl IntoIterator<Item = InstallationRepositories>,
    installation_id: u64,
    preferred: MergeMethod,
    synced_at: u64,
) -> Result<Vec<RepoRecord>, GithubError> {
    let mut total = None;
    let mut repositories = Vec::new();
    for page in pages {
        let total = *total.get_or_insert(page.total_count);
        if page.total_count != total {
            return Err(GithubError::Shifted {
                listing: "installation repositories",
                detail: format!(
                    "the total moved from {total} to {} between pages",
                    page.total_count
                ),
            });
        }
        repositories.extend(page.repositories.into_iter().map(|repo| RepoRecord {
            repository_id: repo.id,
            installation_id,
            owner: repo.owner.login,
            repo: repo.name,
            merge_method: repo.merge_settings.method_instead_of(preferred),
            synced_at,
        }));
    }
    let total = total.unwrap_or(0);
    let mut seen = HashSet::new();
    repositories.retain(|repository| seen.insert(repository.repository_id));
    if repositories.len() != total {
        return Err(GithubError::Shifted {
            listing: "installation repositories",
            detail: format!(
                "{} distinct repositories were listed against a total of {total}",
                repositories.len()
            ),
        });
    }
    Ok(repositories)
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const INSTALLATION_ID: u64 = 42;
    const SYNCED_AT: u64 = 1_700_000_000;

    fn repository(id: u64) -> Value {
        json!({ "id": id, "name": format!("repo-{id}"), "owner": { "login": "acme" } })
    }

    /// One page of `GET /installation/repositories`, as GitHub shapes it: the page's
    /// repositories beside the installation's total.
    fn page(total_count: usize, ids: impl IntoIterator<Item = u64>) -> InstallationRepositories {
        serde_json::from_value(json!({
            "total_count": total_count,
            "repositories": ids.into_iter().map(repository).collect::<Vec<_>>(),
        }))
        .expect("a page as GitHub answers with it")
    }

    fn fold(
        pages: impl IntoIterator<Item = InstallationRepositories>,
    ) -> Result<Vec<RepoRecord>, GithubError> {
        fold_repository_pages(pages, INSTALLATION_ID, MergeMethod::Squash, SYNCED_AT)
    }

    #[test]
    fn pages_fold_into_records_in_listing_order() {
        let repositories = fold([page(103, 1..=100), page(103, 101..=103)]).unwrap();

        assert_eq!(repositories.len(), 103);
        assert_eq!(
            repositories[0],
            RepoRecord {
                repository_id: 1,
                installation_id: INSTALLATION_ID,
                owner: "acme".to_owned(),
                repo: "repo-1".to_owned(),
                merge_method: None,
                synced_at: SYNCED_AT,
            }
        );
        assert_eq!(repositories[102].repository_id, 103);
    }

    #[test]
    fn a_listing_whose_total_moved_between_pages_is_refused_as_shifted() {
        // Repository 50 is removed from the installation after the first page is served, so
        // the second page starts one position early: 101 is never listed, and the total
        // GitHub reports has moved. Offset pagination cannot say which one went.
        let error = fold([page(150, 1..=100), page(149, 102..=150)]).unwrap_err();

        assert!(
            matches!(
                &error,
                GithubError::Shifted { listing: "installation repositories", detail }
                    if detail == "the total moved from 150 to 149 between pages"
            ),
            "a listing that moved under its pages is not an authoritative set: {error:?}"
        );
    }

    #[test]
    fn a_listing_whose_pages_do_not_add_up_to_the_total_is_refused_as_shifted() {
        // One repository leaves the tail and another joins the head between the pages: the
        // total stands, but the page boundary shifted and repository 100 is listed twice
        // while the newcomer, sorted before it, is never seen.
        let error = fold([page(101, 1..=100), page(101, [100])]).unwrap_err();

        assert!(
            matches!(
                &error,
                GithubError::Shifted { listing: "installation repositories", detail }
                    if detail == "100 distinct repositories were listed against a total of 101"
            ),
            "a duplicate at the boundary means something else was dropped: {error:?}"
        );
    }
}
