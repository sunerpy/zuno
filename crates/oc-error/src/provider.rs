//! Failures reported by a model provider.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::time::Duration;

/// A failure from a model provider, classified by what recovery it permits.
///
/// Every variant is a recovery class, not a description. A caller decides what to
/// do by matching the variant and reading its fields — never by inspecting
/// [`std::fmt::Display`] output. See the crate documentation for why that rule
/// exists and what it costs when it is broken.
///
/// Rendered text is for humans and logs. If a recovery decision needs a piece of
/// information, that information is a field.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The prompt exceeded the model's context window.
    ///
    /// Retrying unchanged fails identically; the conversation must be compacted
    /// first. `limit_tokens` and `used_tokens` carry whatever the provider
    /// reported so a compactor can size its work instead of guessing — exactly
    /// the data that is unrecoverable once a failure has been flattened into a
    /// string.
    #[error("context limit exceeded (used={used_tokens:?} limit={limit_tokens:?})")]
    ContextLimit {
        limit_tokens: Option<u64>,
        used_tokens: Option<u64>,
    },

    /// The provider asked the caller to slow down.
    ///
    /// `retry_after` is the delay the provider itself named, propagated from the
    /// wire. A provider that names no delay yields `None` and the caller applies
    /// its own backoff. A message-parsing classifier cannot recover this value at
    /// all, which is why it is the one field this taxonomy mandates.
    #[error("rate limited by provider (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },

    /// A transport fault or a server-side error expected to clear on its own: a
    /// dropped connection, a 5xx, an overloaded upstream.
    #[error("transient provider failure (status={status:?})")]
    Transient {
        status: Option<u16>,
        #[source]
        source: Option<BoxSource>,
    },

    /// Credentials were missing, expired, or rejected.
    ///
    /// `provider` names whose credentials to refresh, so the recovery path does
    /// not have to guess which of several configured providers failed.
    #[error("authentication rejected by provider {provider}")]
    Auth {
        provider: String,
        #[source]
        source: Option<BoxSource>,
    },

    /// The model declined to answer: a content filter, a safety policy, or a
    /// refusal stop reason.
    ///
    /// `provider_text` is the provider's own wording, carried verbatim for
    /// display. It is payload, never a classification channel — the variant has
    /// already established that this request cannot succeed.
    #[error("provider {provider} refused the request")]
    Refused {
        provider: String,
        provider_text: Option<String>,
    },

    /// A failure no retry can fix: a malformed request, an unknown model, a
    /// protocol violation, a revoked account.
    #[error("unrecoverable provider failure (status={status:?})")]
    Fatal {
        status: Option<u16>,
        #[source]
        source: Option<BoxSource>,
    },
}

impl ProviderError {
    /// Classify a wire status code into the taxonomy.
    ///
    /// This is the single place a status code becomes a recovery class, so the
    /// five provider crates cannot drift apart the way five copies of a
    /// `message.contains("503 service unavailable")` check inevitably do.
    ///
    /// It is a floor, not the final word. A provider crate that can read a richer
    /// signal out of the *response body* — a token count, a
    /// `context_length_exceeded` code, a `Retry-After` header — should build the
    /// more specific variant directly. Parsing a response body is reading the
    /// wire; parsing a rendered error message is not, and only the latter is
    /// forbidden.
    #[must_use]
    pub fn from_status(provider: &str, status: u16) -> Self {
        match status {
            401 | 403 => Self::Auth {
                provider: provider.to_owned(),
                source: None,
            },
            429 => Self::RateLimited { retry_after: None },
            408 | 425 | 500..=599 => Self::Transient {
                status: Some(status),
                source: None,
            },
            _ => Self::Fatal {
                status: Some(status),
                source: None,
            },
        }
    }

    /// A transport-level fault expected to clear on its own.
    pub fn transient(source: impl Into<BoxSource>) -> Self {
        Self::Transient {
            status: None,
            source: Some(source.into()),
        }
    }

    /// A fault no retry can fix.
    pub fn fatal(source: impl Into<BoxSource>) -> Self {
        Self::Fatal {
            status: None,
            source: Some(source.into()),
        }
    }

    /// The action this failure calls for.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when sending the identical request again may succeed.
    ///
    /// [`ProviderError::ContextLimit`] is **not** retryable: the same request
    /// overflows the same window every time. It is *recoverable*, via
    /// [`Recovery::Compact`], and conflating the two is what makes a retry loop
    /// spin until it exhausts its attempt budget.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }

    /// The delay the provider itself asked for, if it named one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            Self::ContextLimit { .. }
            | Self::Transient { .. }
            | Self::Auth { .. }
            | Self::Refused { .. }
            | Self::Fatal { .. } => None,
        }
    }
}

impl Recoverable for ProviderError {
    /// Each variant is listed explicitly rather than collapsed into a `_ =>` arm.
    /// That is deliberate: adding a variant must break this function so its
    /// author has to decide what the new failure means, instead of silently
    /// inheriting a wildcard's answer.
    fn recovery(&self) -> Recovery {
        match self {
            Self::ContextLimit { .. } => Recovery::Compact,
            Self::RateLimited { retry_after } => Recovery::Retry {
                after: *retry_after,
            },
            Self::Transient { .. } => Recovery::Retry { after: None },
            Self::Auth { .. } => Recovery::Reauthenticate,
            Self::Refused { .. } | Self::Fatal { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_returns_the_duration_the_provider_sent() {
        let e = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(e.retry_after(), Some(Duration::from_secs(30)));
        assert!(e.is_retryable());
        assert_eq!(
            e.recovery(),
            Recovery::Retry {
                after: Some(Duration::from_secs(30))
            }
        );
    }

    #[test]
    fn rate_limited_without_a_named_delay_is_still_retryable() {
        let e = ProviderError::RateLimited { retry_after: None };
        assert_eq!(e.retry_after(), None);
        assert!(e.is_retryable());
    }

    #[test]
    fn context_limit_asks_for_compaction_not_retry() {
        let e = ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311),
        };
        assert_eq!(e.recovery(), Recovery::Compact);
        assert!(!e.is_retryable());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn context_limit_carries_the_numbers_a_compactor_needs() {
        let e = ProviderError::ContextLimit {
            limit_tokens: Some(200_000),
            used_tokens: Some(214_311),
        };
        let ProviderError::ContextLimit {
            limit_tokens,
            used_tokens,
        } = e
        else {
            panic!("constructed a ContextLimit, matched something else");
        };
        assert_eq!(limit_tokens, Some(200_000));
        assert_eq!(used_tokens, Some(214_311));
    }

    #[test]
    fn transient_is_retryable_with_caller_chosen_backoff() {
        let e = ProviderError::Transient {
            status: Some(503),
            source: None,
        };
        assert!(e.is_retryable());
        assert_eq!(e.retry_after(), None);
    }

    #[test]
    fn auth_asks_for_reauthentication_and_names_the_provider() {
        let e = ProviderError::Auth {
            provider: "anthropic".to_owned(),
            source: None,
        };
        assert_eq!(e.recovery(), Recovery::Reauthenticate);
        assert!(!e.is_retryable());
        assert_eq!(
            e.to_string(),
            "authentication rejected by provider anthropic"
        );
    }

    #[test]
    fn refused_and_fatal_are_terminal() {
        let refused = ProviderError::Refused {
            provider: "openai".to_owned(),
            provider_text: Some("I can't help with that".to_owned()),
        };
        let fatal = ProviderError::Fatal {
            status: Some(400),
            source: None,
        };
        assert_eq!(refused.recovery(), Recovery::Fail);
        assert_eq!(fatal.recovery(), Recovery::Fail);
        assert!(!refused.is_retryable());
        assert!(!fatal.is_retryable());
    }

    #[test]
    fn status_classification_covers_the_codes_message_matching_used_to_chase() {
        let cases: &[(u16, Recovery)] = &[
            (401, Recovery::Reauthenticate),
            (403, Recovery::Reauthenticate),
            (408, Recovery::Retry { after: None }),
            (425, Recovery::Retry { after: None }),
            (429, Recovery::Retry { after: None }),
            (500, Recovery::Retry { after: None }),
            (502, Recovery::Retry { after: None }),
            (503, Recovery::Retry { after: None }),
            (504, Recovery::Retry { after: None }),
            (529, Recovery::Retry { after: None }),
            (400, Recovery::Fail),
            (404, Recovery::Fail),
            (422, Recovery::Fail),
        ];
        for &(status, expected) in cases {
            let actual = ProviderError::from_status("anthropic", status).recovery();
            assert_eq!(actual, expected, "status {status} classified wrongly");
        }
    }

    #[test]
    fn status_429_is_rate_limited_rather_than_merely_transient() {
        assert!(matches!(
            ProviderError::from_status("anthropic", 429),
            ProviderError::RateLimited { retry_after: None }
        ));
    }

    #[test]
    fn constructors_chain_the_underlying_cause() {
        use std::error::Error as _;

        let transient = ProviderError::transient(std::io::Error::other("connection reset"));
        assert_eq!(
            transient.source().map(ToString::to_string).as_deref(),
            Some("connection reset")
        );
        assert!(transient.is_retryable());

        let fatal = ProviderError::fatal(std::io::Error::other("unknown model"));
        assert_eq!(
            fatal.source().map(ToString::to_string).as_deref(),
            Some("unknown model")
        );
        assert!(!fatal.is_retryable());
    }

    #[test]
    fn inherent_and_trait_recovery_agree_on_every_variant() {
        let errors = [
            ProviderError::ContextLimit {
                limit_tokens: None,
                used_tokens: None,
            },
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
            ProviderError::Transient {
                status: None,
                source: None,
            },
            ProviderError::Auth {
                provider: "google".to_owned(),
                source: None,
            },
            ProviderError::Refused {
                provider: "google".to_owned(),
                provider_text: None,
            },
            ProviderError::Fatal {
                status: None,
                source: None,
            },
        ];
        for e in &errors {
            assert_eq!(e.recovery(), Recoverable::recovery(e), "{e}");
            assert_eq!(e.is_retryable(), Recoverable::is_retryable(e), "{e}");
            assert_eq!(e.retry_after(), Recoverable::retry_after(e), "{e}");
        }
    }
}
