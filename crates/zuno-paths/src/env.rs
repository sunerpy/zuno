//! The environment inputs that decide where everything lives.
//!
//! # Why the environment is a value instead of a global read
//!
//! The oracle reads `process.env` directly. A Rust port cannot copy that shape
//! and stay testable: `std::env::set_var` is `unsafe` in edition 2024, and this
//! workspace sets `unsafe_code = "forbid"`, so no test in this crate is allowed
//! to mutate the process environment. Threading an [`Env`] value through
//! instead makes every layout question a pure function of explicit data — which
//! is also what lets the differential test hand the *same* map to a child
//! process and to [`crate::Layout::resolve`] and compare the two byte for byte.
//!
//! # The two JavaScript truthiness rules, kept apart on purpose
//!
//! The oracle mixes `||` and `??`, and the difference is observable:
//!
//! - `xdg-basedir` uses `env.XDG_DATA_HOME || join(home, ".local", "share")`,
//!   so an **empty** `XDG_DATA_HOME` falls back. Verified against the 1.18.12
//!   binary: `XDG_DATA_HOME= opencode debug paths` still prints
//!   `/config/.local/share/opencode`.
//! - `Global.Path.home` uses `ZUNO_TEST_HOME` with nullish semantics, so an
//!   **empty** value is used as-is.
//!
//! [`Env::truthy_value`] models the first rule and [`Env::value`] the second.
//! Reaching for the wrong one silently relocates a user's whole data directory,
//! so the names are deliberately not interchangeable.

use std::collections::BTreeMap;

/// The HTTP Basic username used when `ZUNO_SERVER_USERNAME` is absent.
pub const DEFAULT_SERVER_USERNAME: &str = "zuno";

/// `XDG_DATA_HOME`.
pub const XDG_DATA_HOME: &str = "XDG_DATA_HOME";
/// `XDG_CACHE_HOME`.
pub const XDG_CACHE_HOME: &str = "XDG_CACHE_HOME";
/// `XDG_CONFIG_HOME`.
pub const XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
/// `XDG_STATE_HOME`.
pub const XDG_STATE_HOME: &str = "XDG_STATE_HOME";
/// `HOME`, the POSIX home directory.
pub const HOME: &str = "HOME";
/// `TMPDIR`, the first key Node's `os.tmpdir()` consults on POSIX.
pub const TMPDIR: &str = "TMPDIR";
/// `TMP`, the second key Node's `os.tmpdir()` consults on POSIX.
pub const TMP: &str = "TMP";
/// `TEMP`, the third key Node's `os.tmpdir()` consults on POSIX.
pub const TEMP: &str = "TEMP";
/// `ZUNO_TEST_HOME`, which overrides `home` with nullish semantics.
pub const ZUNO_TEST_HOME: &str = "ZUNO_TEST_HOME";
/// `ZUNO_CONFIG_DIR`, an extra configuration directory.
pub const ZUNO_CONFIG_DIR: &str = "ZUNO_CONFIG_DIR";
/// `ZUNO_DISABLE_PROJECT_CONFIG`, which drops the project `.zuno` chain.
pub const ZUNO_DISABLE_PROJECT_CONFIG: &str = "ZUNO_DISABLE_PROJECT_CONFIG";
/// `ZUNO_DB`, the database path override.
pub const ZUNO_DB: &str = "ZUNO_DB";
/// `ZUNO_DISABLE_CHANNEL_DB`, which forces the unsuffixed database name.
pub const ZUNO_DISABLE_CHANNEL_DB: &str = "ZUNO_DISABLE_CHANNEL_DB";
/// `ZUNO_MODELS_URL`, the model catalog source.
pub const ZUNO_MODELS_URL: &str = "ZUNO_MODELS_URL";

/// An immutable snapshot of the environment variables that affect paths.
///
/// `BTreeMap` rather than `HashMap` so that [`Env::iter`] is deterministic; a
/// differential test that builds a child process environment from this needs a
/// stable order to be reproducible.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    /// Snapshot the current process environment.
    ///
    /// Uses `vars_os` and lossy conversion rather than `vars`, which panics on
    /// a non-UTF-8 variable. The oracle reads `process.env` as JavaScript
    /// strings, so a byte sequence that is not valid UTF-8 is already mangled
    /// there; matching that with a lossy conversion is closer to parity than
    /// aborting. See the project's engineering notes.
    #[must_use]
    pub fn from_process() -> Self {
        let vars = std::env::vars_os()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
        Self { vars }
    }

    /// An environment with nothing set.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build an [`Env`] from explicit pairs.
    #[must_use]
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let vars = pairs
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        Self { vars }
    }

    /// Set `key`, consuming and returning `self` so calls chain.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    /// Unset `key`, consuming and returning `self` so calls chain.
    #[must_use]
    pub fn without(mut self, key: &str) -> Self {
        self.vars.remove(key);
        self
    }

    /// The raw value, present even when empty — JavaScript's `??` semantics.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// The value only when it is non-empty — JavaScript's `||` semantics.
    #[must_use]
    pub fn truthy_value(&self, key: &str) -> Option<&str> {
        self.value(key).filter(|value| !value.is_empty())
    }

    /// Port of `Flag.truthy`: true when the value lower-cases to `"true"` or
    /// `"1"`.
    ///
    /// Note that `ZUNO_DISABLE_CHANNEL_DB` does **not** go through this —
    /// `database.ts` compares the raw value against `"1"` and `"true"` without
    /// lower-casing, so `TRUE` enables the flag here but not there. See
    /// [`Env::exact_flag`].
    #[must_use]
    pub fn flag(&self, key: &str) -> bool {
        self.value(key)
            .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1")
    }

    /// Case-sensitive flag test, as `database.ts:50-52` writes it.
    #[must_use]
    pub fn exact_flag(&self, key: &str) -> bool {
        matches!(self.value(key), Some("1" | "true"))
    }

    /// Every variable, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Number of variables set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// Whether no variable is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_value_rejects_empty_and_value_keeps_it() {
        let env = Env::empty()
            .with(XDG_DATA_HOME, "")
            .with(ZUNO_TEST_HOME, "");
        assert_eq!(env.truthy_value(XDG_DATA_HOME), None);
        assert_eq!(env.value(ZUNO_TEST_HOME), Some(""));
        assert_eq!(env.value("MISSING"), None);
        assert_eq!(env.truthy_value("MISSING"), None);
    }

    #[test]
    fn flag_lowercases_but_exact_flag_does_not() {
        for value in ["true", "TRUE", "True", "1"] {
            assert!(Env::empty().with("K", value).flag("K"), "flag({value:?})");
        }
        for value in ["", "0", "yes", "false"] {
            assert!(!Env::empty().with("K", value).flag("K"), "flag({value:?})");
        }
        assert!(Env::empty().with("K", "true").exact_flag("K"));
        assert!(Env::empty().with("K", "1").exact_flag("K"));
        assert!(!Env::empty().with("K", "TRUE").exact_flag("K"));
        assert!(!Env::empty().with("K", "True").exact_flag("K"));
    }

    #[test]
    fn builders_compose() {
        let env = Env::from_pairs([("A", "1"), ("B", "2")])
            .with("C", "3")
            .without("A");
        assert_eq!(env.len(), 2);
        assert!(!env.is_empty());
        assert_eq!(env.iter().collect::<Vec<_>>(), vec![("B", "2"), ("C", "3")]);
        assert!(Env::empty().is_empty());
    }

    #[test]
    fn from_process_sees_the_real_environment() {
        let env = Env::from_process();
        assert_eq!(env.value("PATH"), std::env::var("PATH").ok().as_deref());
    }

    #[test]
    fn environment_keys_are_read_without_aliases() {
        let env = Env::empty().with("ZUNO_DB", "database.sqlite");
        assert_eq!(env.value("ZUNO_DB"), Some("database.sqlite"));
        assert_eq!(env.value("OPENCODE_DB"), None);
    }
}
