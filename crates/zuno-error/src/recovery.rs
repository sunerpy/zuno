//! What a caller should do next, decided from an error's shape alone.

use std::time::Duration;

/// The complete set of recovery actions any layer of this workspace may take in
/// response to a failure.
///
/// This exists so that no caller ever has to look at a rendered error message to
/// decide what to do. Every error type in this crate answers
/// [`Recoverable::recovery`] by matching on its own variants and reading its own
/// fields, so the decision is made once, in the type, and is checked by the
/// compiler.
///
/// The enum is deliberately **not** `#[non_exhaustive]`. Every consumer lives in
/// this workspace, so an exhaustive `match` is a feature: adding an action here
/// breaks every recovery site until its author makes an explicit decision. A
/// forced `_ =>` arm would silently route new actions to whatever the wildcard
/// happened to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Recovery {
    /// Send the identical request again.
    ///
    /// `after` is the delay the peer explicitly asked for, propagated from the
    /// wire (a `Retry-After` header, a `retry_after` field in an error body).
    /// `None` means the peer named no delay and the caller should apply its own
    /// backoff policy.
    Retry { after: Option<Duration> },
    /// The request is too large to succeed. It must be made smaller — by
    /// compacting the conversation or dropping context — before another attempt.
    /// Retrying it unchanged fails identically.
    Compact,
    /// Credentials are missing, expired, or rejected. A human or a refresh flow
    /// must supply new ones; no number of retries will help.
    Reauthenticate,
    /// Nothing the caller can do will make this request succeed. Surface it.
    Fail,
}

impl Recovery {
    /// True when the identical request may be sent again.
    #[must_use]
    pub const fn is_retry(self) -> bool {
        matches!(self, Self::Retry { .. })
    }

    /// The delay the peer asked for, if it named one.
    #[must_use]
    pub const fn retry_after(self) -> Option<Duration> {
        match self {
            Self::Retry { after } => after,
            Self::Compact | Self::Reauthenticate | Self::Fail => None,
        }
    }
}

/// Implemented by every error type in this crate so that generic retry and
/// reporting helpers can route any failure without knowing its domain.
///
/// Implementors provide only [`Recoverable::recovery`]; the two predicates are
/// derived from it, which makes it impossible for them to disagree.
///
/// Domain error types also expose `recovery`, `is_retryable` and `retry_after`
/// as inherent methods with identical behaviour, so callers holding a concrete
/// error need no import.
pub trait Recoverable {
    /// The action this failure calls for, decided from variant and fields.
    fn recovery(&self) -> Recovery;

    /// True when the identical request may be sent again.
    fn is_retryable(&self) -> bool {
        self.recovery().is_retry()
    }

    /// The delay the peer asked for, if it named one.
    fn retry_after(&self) -> Option<Duration> {
        self.recovery().retry_after()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_retry_is_retryable() {
        assert!(Recovery::Retry { after: None }.is_retry());
        assert!(!Recovery::Compact.is_retry());
        assert!(!Recovery::Reauthenticate.is_retry());
        assert!(!Recovery::Fail.is_retry());
    }

    #[test]
    fn retry_after_reads_the_carried_duration() {
        let thirty = Duration::from_secs(30);
        assert_eq!(
            Recovery::Retry {
                after: Some(thirty)
            }
            .retry_after(),
            Some(thirty)
        );
        assert_eq!(Recovery::Retry { after: None }.retry_after(), None);
        assert_eq!(Recovery::Compact.retry_after(), None);
    }
}
