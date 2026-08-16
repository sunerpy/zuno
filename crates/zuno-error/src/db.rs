//! Failures from the local session store.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::path::PathBuf;
use std::time::Duration;

/// A failure from the local session store.
///
/// [`DbError::Busy`] is the variant that earns this enum its keep: "database is
/// locked" is a transient condition that clears on its own, and the only way a
/// caller can tell it apart from a permanent failure without this variant is to
/// match on the text of a message.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A database exists under Zuno's pre-rename filename and must be moved explicitly.
    #[error(
        "legacy Zuno database {old_path} was not opened; move it to {new_path} before continuing"
    )]
    LegacyDatabase {
        old_path: PathBuf,
        new_path: PathBuf,
    },

    /// The database file could not be opened or created.
    #[error("database {path} could not be opened")]
    Open {
        path: PathBuf,
        #[source]
        source: BoxSource,
    },

    /// A schema migration failed. `version` is the migration that failed, so a
    /// repair path knows where the schema actually stopped.
    #[error("migration to schema version {version} failed")]
    Migration {
        version: u32,
        #[source]
        source: BoxSource,
    },

    /// The journal was written by a binary with a newer migration set.
    #[error(
        "database migration journal is newer than this binary (known ceiling {ceiling}, observed {observed})"
    )]
    MigrationTooNew { ceiling: String, observed: String },

    /// A statement failed to execute.
    #[error("database statement failed")]
    Query {
        #[source]
        source: BoxSource,
    },

    /// Another connection or process holds the write lock. Retryable.
    #[error("database is locked by another writer (retry_after={retry_after:?})")]
    Busy { retry_after: Option<Duration> },

    /// The row was expected to exist and does not.
    #[error("no row in {table} with id {id}")]
    NotFound { table: String, id: String },

    /// A stored value could not be decoded into the type the schema promises.
    /// This is corruption or a schema/code mismatch, never a transient fault.
    #[error("stored value in {table} could not be decoded")]
    Decode {
        table: String,
        #[source]
        source: serde_json::Error,
    },
}

impl DbError {
    /// The action this failure calls for.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when running the identical statement again may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }

    /// The delay the store asked for, if it named one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Busy { retry_after } => *retry_after,
            Self::LegacyDatabase { .. }
            | Self::Open { .. }
            | Self::Migration { .. }
            | Self::MigrationTooNew { .. }
            | Self::Query { .. }
            | Self::NotFound { .. }
            | Self::Decode { .. } => None,
        }
    }
}

impl Recoverable for DbError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Busy { retry_after } => Recovery::Retry {
                after: *retry_after,
            },
            Self::LegacyDatabase { .. }
            | Self::Open { .. }
            | Self::Migration { .. }
            | Self::MigrationTooNew { .. }
            | Self::Query { .. }
            | Self::NotFound { .. }
            | Self::Decode { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_is_the_only_retryable_variant() {
        let errors = [
            DbError::LegacyDatabase {
                old_path: PathBuf::from("opencode.db"),
                new_path: PathBuf::from("zuno.db"),
            },
            DbError::Open {
                path: PathBuf::from("opencode.db"),
                source: Box::new(std::io::Error::other("permission denied")),
            },
            DbError::Migration {
                version: 7,
                source: Box::new(std::io::Error::other("no such column")),
            },
            DbError::Query {
                source: Box::new(std::io::Error::other("syntax error")),
            },
            DbError::Busy {
                retry_after: Some(Duration::from_millis(50)),
            },
            DbError::NotFound {
                table: "message".to_owned(),
                id: "msg_01".to_owned(),
            },
            DbError::Decode {
                table: "message".to_owned(),
                source: serde_json::from_str::<u8>("{}").unwrap_err(),
            },
        ];
        for e in &errors {
            let expected = matches!(e, DbError::Busy { .. });
            assert_eq!(e.is_retryable(), expected, "{e}");
        }
    }

    #[test]
    fn busy_propagates_the_delay_it_was_given() {
        let e = DbError::Busy {
            retry_after: Some(Duration::from_millis(50)),
        };
        assert_eq!(e.retry_after(), Some(Duration::from_millis(50)));
        assert_eq!(
            e.recovery(),
            Recovery::Retry {
                after: Some(Duration::from_millis(50))
            }
        );
    }

    #[test]
    fn migration_names_the_version_that_failed() {
        let e = DbError::Migration {
            version: 7,
            source: Box::new(std::io::Error::other("no such column")),
        };
        assert_eq!(e.to_string(), "migration to schema version 7 failed");
        let DbError::Migration { version, .. } = &e else {
            panic!("constructed a Migration, matched something else");
        };
        assert_eq!(*version, 7);
    }

    #[test]
    fn migration_too_new_names_the_known_ceiling_and_observed_id() {
        let e = DbError::MigrationTooNew {
            ceiling: "20260622202450_simplify_session_input".to_owned(),
            observed: "99999999999999_future_migration".to_owned(),
        };
        let message = e.to_string();
        assert!(
            message.contains("20260622202450_simplify_session_input"),
            "{message}"
        );
        assert!(
            message.contains("99999999999999_future_migration"),
            "{message}"
        );
        assert!(!e.is_retryable());
    }

    #[test]
    fn not_found_identifies_the_missing_row() {
        let e = DbError::NotFound {
            table: "session".to_owned(),
            id: "ses_01".to_owned(),
        };
        assert_eq!(e.to_string(), "no row in session with id ses_01");
        assert!(!e.is_retryable());
    }
}
