//! What a call to GitHub can fail with, and the mark that says whether the
//! failure came back from a resource the client had already read.

use dependaboard_core::{GithubErrorResponse, Operation};
use reqwest::StatusCode;
use thiserror::Error;

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
    /// A paged listing moved under its own pages: the total GitHub reports changed
    /// between them, or the pages did not add up to it. Offset pagination cannot say
    /// which item a boundary shift dropped, so the set fetched must not be treated as
    /// authoritative. Safe to retry: a fresh listing starts over from the first page.
    #[error("GitHub {listing} listing shifted while it was being paged: {detail}")]
    Shifted {
        listing: &'static str,
        detail: String,
    },
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
    /// A well-formed GraphQL answer that reported no error yet lacks a part the query
    /// asked for, such as a pull request with no head commit.
    #[error("GitHub GraphQL answer is missing its {0}")]
    Missing(&'static str),
}

pub(crate) fn not_found(message: &str) -> GithubError {
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
pub(crate) fn known_http(error: GithubError) -> GithubError {
    match error {
        GithubError::Http { response, .. } => GithubError::Http {
            response,
            known_resource: true,
        },
        error => error,
    }
}
