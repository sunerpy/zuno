//! Password-gated HTTP Basic authentication.
//!
//! Authentication is disabled when `ZUNO_SERVER_PASSWORD` is absent or empty.
//! A configured username may be empty; only an absent username receives
//! [`zuno_paths::env::DEFAULT_SERVER_USERNAME`].

use axum::http::HeaderMap;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

pub(crate) const WWW_AUTHENTICATE_VALUE: &str = "Basic realm=\"Secure Area\"";

/// Authentication settings resolved before the listener is bound.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthConfig {
    username: String,
    password: Option<String>,
}

impl AuthConfig {
    /// Creates settings from the two Zuno environment values.
    ///
    /// `None` means the variable was absent. `Some("")` is retained as data but
    /// disables authentication exactly like absence.
    #[must_use]
    pub fn new(password: Option<String>, username: Option<String>) -> Self {
        Self {
            username: username
                .unwrap_or_else(|| zuno_paths::env::DEFAULT_SERVER_USERNAME.to_owned()),
            password,
        }
    }

    /// Reads `ZUNO_SERVER_PASSWORD` and `ZUNO_SERVER_USERNAME` once.
    #[must_use]
    pub fn from_env() -> Self {
        let password = std::env::var_os("ZUNO_SERVER_PASSWORD")
            .map(|value| value.to_string_lossy().into_owned());
        let username = std::env::var_os("ZUNO_SERVER_USERNAME")
            .map(|value| value.to_string_lossy().into_owned());
        Self::new(password, username)
    }

    /// Whether every route must require Basic credentials.
    #[must_use]
    pub fn required(&self) -> bool {
        self.password
            .as_ref()
            .is_some_and(|password| !password.is_empty())
    }

    /// The configured username, defaulting to `zuno` only when unset.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn authorizes(&self, headers: &HeaderMap) -> bool {
        if !self.required() {
            return true;
        }
        self.authorizes_basic(headers)
    }

    pub(crate) fn authorizes_basic(&self, headers: &HeaderMap) -> bool {
        if !self.required() {
            return false;
        }
        let Some((username, password)) = basic_credentials(headers) else {
            return false;
        };
        self.password
            .as_ref()
            .is_some_and(|expected| username == self.username && password == *expected)
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthConfig")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("required", &self.required())
            .finish()
    }
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let separator = value.find(char::is_whitespace)?;
    let (scheme, encoded) = value.split_at(separator);
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let encoded = encoded.trim_start_matches(char::is_whitespace);
    if encoded.is_empty() {
        return None;
    }
    let decoded = STANDARD.decode(encoded).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_owned(), password.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_contains_the_password() {
        let rendered = format!(
            "{:?}",
            AuthConfig::new(Some("never-print-this".to_owned()), None)
        );
        assert!(!rendered.contains("never-print-this"));
        assert!(rendered.contains("[REDACTED]"));
    }
}
