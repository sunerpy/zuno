//! [`Secret`] — a string that cannot leak through a log line.
//!
//! # Why a wrapper and not a discipline
//!
//! Every field in this crate that holds a refresh token, an access token, an API
//! key, a client secret, or a PKCE verifier is a `Secret`. That is not a naming
//! convention; it is enforced by the type, because the two ways a credential
//! actually escapes a process are both automatic:
//!
//! - `#[derive(Debug)]` on the struct that contains it, and then any `{:?}` —
//!   a `tracing` event, a `dbg!`, an `assert_eq!` failure message, a panic
//!   payload, an `unwrap()` on a `Result<_, SomethingContainingIt>`.
//! - `{}` in a format string, when the author reached for `Display` to build a
//!   message and the field happened to be a bare `String`.
//!
//! A plain `String` participates in both. A `Secret` renders
//! [`REDACTED`] through both, so the leak has to be written on purpose —
//! [`Secret::expose`] is the only way out, it is named to be conspicuous at a
//! call site, and it is greppable in review.
//!
//! # What it is not
//!
//! It is not encryption, and it does not scrub memory on drop: the value sits in
//! the process heap in plaintext, exactly as the file on disk does. The threat it
//! answers is *accidental disclosure through the crate's own output* — a log
//! file, a crash report, a bug-report paste — which is the one that actually
//! happens.
//!
//! [`PartialEq`] is byte-comparing and therefore not constant-time. Nothing here
//! compares a stored credential against attacker-supplied input; the impl exists
//! so round-trip tests can assert a token survived a write intact.

use std::fmt;

/// What a `Secret` renders as through `Debug` and `Display`.
pub const REDACTED: &str = "<redacted>";

/// The `…` used by [`Secret::hint`] between the prefix and the suffix.
const HINT_ELLIPSIS: char = '…';

/// Leading characters [`Secret::hint`] keeps.
const HINT_PREFIX: usize = 3;

/// Trailing characters [`Secret::hint`] keeps.
const HINT_SUFFIX: usize = 4;

/// Shortest value [`Secret::hint`] will partially reveal. Below this a hint
/// would expose most of the value, so it redacts entirely.
const HINT_MINIMUM: usize = 12;

/// A credential string that renders as [`REDACTED`] through both `Debug` and
/// `Display`.
///
/// Serializes and deserializes as a bare JSON string, so a struct field of this
/// type is on-disk-identical to the `String` it replaces.
///
/// ```
/// use zuno_auth::Secret;
///
/// let key = Secret::new("sk-live-abcdefgh1234");
/// assert_eq!(format!("{key:?}"), "<redacted>");
/// assert_eq!(format!("{key}"), "<redacted>");
/// assert_eq!(key.expose(), "sk-live-abcdefgh1234");
/// ```
#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wrap a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The plaintext.
    ///
    /// The deliberately awkward name is the point: every disclosure of a
    /// credential in this workspace is one `expose()` that a reviewer can find.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and yield the plaintext.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Whether the value is empty. Safe to log: it reveals no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A partial rendering for a human who has to tell two credentials apart —
    /// `sk-…1234`.
    ///
    /// Opt-in, never used by `Debug` or `Display`, and it refuses to reveal
    /// anything from a value shorter than [`HINT_MINIMUM`] characters, where a
    /// prefix plus a suffix would be most of the secret.
    ///
    /// ```
    /// use zuno_auth::Secret;
    ///
    /// assert_eq!(Secret::new("sk-live-abcdefgh1234").hint(), "sk-…1234");
    /// assert_eq!(Secret::new("short").hint(), "<redacted>");
    /// ```
    #[must_use]
    pub fn hint(&self) -> String {
        let characters: Vec<char> = self.0.chars().collect();
        if characters.len() < HINT_MINIMUM {
            return REDACTED.to_owned();
        }
        let prefix: String = characters.iter().take(HINT_PREFIX).collect();
        let suffix: String = characters[characters.len() - HINT_SUFFIX..]
            .iter()
            .collect();
        format!("{prefix}{HINT_ELLIPSIS}{suffix}")
    }
}

/// Redacted, so a `#[derive(Debug)]` on any enclosing type is safe.
impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

/// Redacted, so reaching for `{}` instead of `{:?}` is not a way around it.
impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAINTEXT: &str = "sk-live-supersecret-9f2a";

    #[test]
    fn debug_and_display_both_redact() {
        let secret = Secret::new(PLAINTEXT);
        assert_eq!(format!("{secret:?}"), REDACTED);
        assert_eq!(format!("{secret}"), REDACTED);
        assert_eq!(format!("{secret:#?}"), REDACTED);
        assert!(!format!("{secret:?} {secret}").contains("supersecret"));
    }

    /// The wrapper exists for this case: an enclosing type derives `Debug` and
    /// someone formats it.
    #[test]
    fn a_derived_debug_on_an_enclosing_type_cannot_leak() {
        #[derive(Debug)]
        struct Enclosing {
            label: &'static str,
            token: Secret,
        }

        let enclosing = Enclosing {
            label: "anthropic",
            token: Secret::new(PLAINTEXT),
        };
        let rendered = format!("{enclosing:?}");
        assert!(rendered.contains("anthropic"), "{rendered}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(rendered.contains(REDACTED), "{rendered}");
        assert_eq!(enclosing.label, "anthropic");
        assert_eq!(enclosing.token.expose(), PLAINTEXT);
    }

    #[test]
    fn expose_is_the_only_way_out() {
        assert_eq!(Secret::new(PLAINTEXT).expose(), PLAINTEXT);
        assert_eq!(Secret::new(PLAINTEXT).into_inner(), PLAINTEXT);
    }

    #[test]
    fn serializes_as_a_bare_string() {
        let json = serde_json::to_string(&Secret::new(PLAINTEXT)).expect("serialize");
        assert_eq!(json, format!("\"{PLAINTEXT}\""));
        let parsed: Secret = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.expose(), PLAINTEXT);
    }

    #[test]
    fn hint_reveals_a_bounded_prefix_and_suffix() {
        assert_eq!(Secret::new(PLAINTEXT).hint(), "sk-…9f2a");
        assert_eq!(Secret::new("abcdefghijkl").hint(), "abc…ijkl");
    }

    #[test]
    fn hint_refuses_short_values() {
        for short in ["", "a", "abcdefghijk"] {
            assert_eq!(
                Secret::new(short).hint(),
                REDACTED,
                "{short:?} should not be partially revealed"
            );
        }
    }

    #[test]
    fn hint_is_char_boundary_safe() {
        // Multi-byte characters would panic a byte-sliced implementation.
        let secret = Secret::new("密码密码密码密码密码密码");
        assert_eq!(secret.hint(), "密码密…密码密码");
    }

    #[test]
    fn is_empty_reports_without_revealing() {
        assert!(Secret::new("").is_empty());
        assert!(!Secret::new(PLAINTEXT).is_empty());
    }
}
