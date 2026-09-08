//! The store's error, and which of its failures a caller may retry: SQLite's
//! contention codes, the one constraint a sync can race, and whatever a
//! remote libSQL reports, since it reports everything the same way.

use dependaboard_core::CursorError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("local database directory could not be created: {0}")]
    Io(#[from] std::io::Error),
    #[error("libSQL operation failed: {0}")]
    Database(#[from] libsql::Error),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Cursor(#[from] CursorError),
    #[error("stored enum value is invalid: {0}")]
    CorruptEnum(String),
    #[error("database integer is out of range")]
    IntegerOverflow,
    #[error("aggregate query did not return a row")]
    MissingScalar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreErrorClass {
    Retryable,
    Terminal,
}

impl StoreError {
    pub fn class(&self) -> StoreErrorClass {
        match self {
            Self::Database(error) if retryable_database_error(error) => StoreErrorClass::Retryable,
            Self::Io(_)
            | Self::Database(_)
            | Self::Json(_)
            | Self::Cursor(_)
            | Self::CorruptEnum(_)
            | Self::IntegerOverflow
            | Self::MissingScalar => StoreErrorClass::Terminal,
        }
    }
}

fn retryable_database_error(error: &libsql::Error) -> bool {
    match error {
        libsql::Error::SqliteFailure(code, _) => retryable_sqlite_code(*code),
        // Only the embedded-replica connection reports this way; a store built
        // with `Builder::new_remote` never does.
        libsql::Error::RemoteSqliteFailure(code, extended_code, _) => {
            retryable_sqlite_code(*code) || retryable_sqlite_code(*extended_code)
        }
        libsql::Error::ConnectionFailed(_) | libsql::Error::WalConflict => true,
        // Every failure a store built with `Builder::new_remote` reports — a
        // transport error, a dropped stream, an HTTP 5xx or 429, and even a
        // server-side SQLITE_BUSY or constraint violation — arrives here, as a
        // boxed `HranaError` whose module libsql (0.9) keeps private, so it can
        // be neither matched nor downcast. Treat the whole variant as
        // transient: every caller that consults the class has a bounded retry,
        // so a request that is genuinely bad costs at most its budget before
        // the same terminal report, while the alternative fails every blip on
        // the first attempt. Revisit if libsql exports the type or a
        // structured code.
        libsql::Error::Hrana(_) => true,
        _ => false,
    }
}

fn retryable_sqlite_code(code: i32) -> bool {
    // Extended SQLite result codes retain the primary result in the low byte.
    // Contention (BUSY, LOCKED, IOERR) clears on its own. Of the constraint
    // codes only the foreign key one does: a pull request can race the sync
    // that inserts its repository. A primary key or unique violation is a
    // programming error, and retrying it would only delay the report.
    matches!(code & 0xff, 5 | 6 | 10) || code == 787 // SQLITE_CONSTRAINT_FOREIGNKEY
}

#[cfg(test)]
mod tests {
    use crate::{
        ProjectionWriter,
        test_support::{pr, test_store},
    };

    use super::*;

    /// The classification tells the foreign key race apart from other
    /// constraint violations by SQLite's extended result code, which only
    /// works if libSQL hands that over rather than the bare primary code (19,
    /// SQLITE_CONSTRAINT, which is terminal). A real violation pins it: a
    /// pull request whose repository has not landed yet is a race with the
    /// repository sync, and worth another try.
    #[tokio::test]
    async fn a_pull_request_arriving_before_its_repository_is_worth_retrying() {
        let (_directory, store) = test_store().await;

        let error = store.upsert_pr(&pr(1, 1, 10)).await.unwrap_err();

        assert!(
            error.to_string().contains("FOREIGN KEY constraint failed"),
            "{error}"
        );
        assert_eq!(error.class(), StoreErrorClass::Retryable);
    }

    #[test]
    fn classifies_structured_sqlite_contention_and_the_foreign_key_race_as_retryable() {
        for code in [5, 6, 10, 5 | (2 << 8), 787] {
            let error = StoreError::Database(libsql::Error::SqliteFailure(code, "busy".into()));
            assert_eq!(error.class(), StoreErrorClass::Retryable, "code {code}");
        }
        // An embedded replica reports the primary and extended codes separately.
        let remote = StoreError::Database(libsql::Error::RemoteSqliteFailure(
            19,
            19 | (3 << 8),
            "constraint".into(),
        ));
        assert_eq!(remote.class(), StoreErrorClass::Retryable);
    }

    /// The messages are what libsql's remote connection puts in `Hrana` for a
    /// transport error, a dropped stream, a server error, a rate limit and a
    /// server-side SQLITE_BUSY; the class rests on the variant alone, for the
    /// reason `retryable_database_error` gives.
    #[test]
    fn classifies_a_remote_store_failure_as_retryable() {
        for message in [
            "http error: `connection reset by peer`",
            "stream closed: `stream expired`",
            "api error: `status=503, body=`",
            "api error: `status=429, body=too many requests`",
            "stream error: `Error { message: \"database is locked\", code: \"SQLITE_BUSY\" }`",
        ] {
            let error = StoreError::Database(libsql::Error::Hrana(message.into()));
            assert_eq!(error.class(), StoreErrorClass::Retryable, "{message}");
        }
    }

    #[test]
    fn classifies_unknown_and_data_errors_as_terminal() {
        let database = StoreError::Database(libsql::Error::Misuse("bad call".into()));
        assert_eq!(database.class(), StoreErrorClass::Terminal);
        let generic_constraint =
            StoreError::Database(libsql::Error::SqliteFailure(19, "constraint".into()));
        assert_eq!(generic_constraint.class(), StoreErrorClass::Terminal);
        // Primary key and unique violations are programming errors: a fresh
        // attempt would hit the same row.
        for code in [1555, 2067] {
            let error =
                StoreError::Database(libsql::Error::SqliteFailure(code, "constraint".into()));
            assert_eq!(error.class(), StoreErrorClass::Terminal, "code {code}");
        }
        let remote_unique = StoreError::Database(libsql::Error::RemoteSqliteFailure(
            19,
            19 | (8 << 8),
            "constraint".into(),
        ));
        assert_eq!(remote_unique.class(), StoreErrorClass::Terminal);
        assert_eq!(
            StoreError::IntegerOverflow.class(),
            StoreErrorClass::Terminal
        );
        assert_eq!(StoreError::MissingScalar.class(), StoreErrorClass::Terminal);
    }
}
