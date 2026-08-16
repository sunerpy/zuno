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

/// OpenCode-prefixed names retained because installed JavaScript plugins read them.
pub const PLUGIN_ABI_ENV_NAMES: [&str; 6] = [
    "OPENCODE_CLIENT",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_DISABLE_CLAUDE_CODE",
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
];

/// Project-owned environment names and their only accepted external spellings.
pub const ZUNO_ENV_NAME_MAP: [(&str, &str); 67] = [
    ("OPENCODE_ALWAYS_NOTIFY_UPDATE", "ZUNO_ALWAYS_NOTIFY_UPDATE"),
    ("OPENCODE_API_KEY", "ZUNO_API_KEY"),
    ("OPENCODE_AUTH_CONTENT", "ZUNO_AUTH_CONTENT"),
    ("OPENCODE_AUTO_HEAP_SNAPSHOT", "ZUNO_AUTO_HEAP_SNAPSHOT"),
    ("OPENCODE_CHANNEL", "ZUNO_CHANNEL"),
    ("OPENCODE_CONFIG", "ZUNO_CONFIG"),
    ("OPENCODE_DB", "ZUNO_DB"),
    ("OPENCODE_DISABLE_AUTOCOMPACT", "ZUNO_DISABLE_AUTOCOMPACT"),
    ("OPENCODE_DISABLE_AUTOUPDATE", "ZUNO_DISABLE_AUTOUPDATE"),
    ("OPENCODE_DISABLE_CHANNEL_DB", "ZUNO_DISABLE_CHANNEL_DB"),
    (
        "OPENCODE_DISABLE_CLAUDE_CODE_PROMPT",
        "ZUNO_DISABLE_CLAUDE_CODE_PROMPT",
    ),
    (
        "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS",
        "ZUNO_DISABLE_CLAUDE_CODE_SKILLS",
    ),
    (
        "OPENCODE_DISABLE_DEFAULT_PLUGINS",
        "ZUNO_DISABLE_DEFAULT_PLUGINS",
    ),
    (
        "OPENCODE_DISABLE_EXTERNAL_SKILLS",
        "ZUNO_DISABLE_EXTERNAL_SKILLS",
    ),
    ("OPENCODE_DISABLE_FFF", "ZUNO_DISABLE_FFF"),
    ("OPENCODE_DISABLE_LSP_DOWNLOAD", "ZUNO_DISABLE_LSP_DOWNLOAD"),
    ("OPENCODE_DISABLE_MODELS_FETCH", "ZUNO_DISABLE_MODELS_FETCH"),
    ("OPENCODE_DISABLE_MOUSE", "ZUNO_DISABLE_MOUSE"),
    (
        "OPENCODE_DISABLE_PROJECT_CONFIG",
        "ZUNO_DISABLE_PROJECT_CONFIG",
    ),
    ("OPENCODE_DISABLE_PRUNE", "ZUNO_DISABLE_PRUNE"),
    (
        "OPENCODE_DISABLE_TERMINAL_TITLE",
        "ZUNO_DISABLE_TERMINAL_TITLE",
    ),
    ("OPENCODE_ENABLE_EXA", "ZUNO_ENABLE_EXA"),
    (
        "OPENCODE_ENABLE_EXPERIMENTAL_MODELS",
        "ZUNO_ENABLE_EXPERIMENTAL_MODELS",
    ),
    ("OPENCODE_ENABLE_JS_PLUGINS", "ZUNO_ENABLE_JS_PLUGINS"),
    ("OPENCODE_ENABLE_PARALLEL", "ZUNO_ENABLE_PARALLEL"),
    ("OPENCODE_ENABLE_QUESTION_TOOL", "ZUNO_ENABLE_QUESTION_TOOL"),
    ("OPENCODE_EXPERIMENTAL", "ZUNO_EXPERIMENTAL"),
    (
        "OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT",
        "ZUNO_EXPERIMENTAL_DISABLE_COPY_ON_SELECT",
    ),
    (
        "OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER",
        "ZUNO_EXPERIMENTAL_DISABLE_FILEWATCHER",
    ),
    ("OPENCODE_EXPERIMENTAL_EXA", "ZUNO_EXPERIMENTAL_EXA"),
    (
        "OPENCODE_EXPERIMENTAL_FILEWATCHER",
        "ZUNO_EXPERIMENTAL_FILEWATCHER",
    ),
    (
        "OPENCODE_EXPERIMENTAL_LSP_TOOL",
        "ZUNO_EXPERIMENTAL_LSP_TOOL",
    ),
    (
        "OPENCODE_EXPERIMENTAL_PARALLEL",
        "ZUNO_EXPERIMENTAL_PARALLEL",
    ),
    (
        "OPENCODE_EXPERIMENTAL_PLAN_MODE",
        "ZUNO_EXPERIMENTAL_PLAN_MODE",
    ),
    (
        "OPENCODE_EXPERIMENTAL_REFERENCES",
        "ZUNO_EXPERIMENTAL_REFERENCES",
    ),
    (
        "OPENCODE_EXPERIMENTAL_WORKSPACES",
        "ZUNO_EXPERIMENTAL_WORKSPACES",
    ),
    ("OPENCODE_FAKE_VCS", "ZUNO_FAKE_VCS"),
    ("OPENCODE_FLAG_NAMES", "ZUNO_FLAG_NAMES"),
    ("OPENCODE_GIT_BASH_PATH", "ZUNO_GIT_BASH_PATH"),
    ("OPENCODE_LEGACY_DB", "ZUNO_LEGACY_DB"),
    ("OPENCODE_LOG_LEVEL", "ZUNO_LOG_LEVEL"),
    ("OPENCODE_MODEL", "ZUNO_MODEL"),
    ("OPENCODE_MODELS_DEV", "ZUNO_MODELS_DEV"),
    ("OPENCODE_MODELS_PATH", "ZUNO_MODELS_PATH"),
    ("OPENCODE_MODELS_URL", "ZUNO_MODELS_URL"),
    ("OPENCODE_PERMISSION", "ZUNO_PERMISSION"),
    ("OPENCODE_PID", "ZUNO_PID"),
    ("OPENCODE_PLUGIN_LOOPBACK_PORT", "ZUNO_PLUGIN_LOOPBACK_PORT"),
    ("OPENCODE_PLUGIN_META_FILE", "ZUNO_PLUGIN_META_FILE"),
    ("OPENCODE_PLUGIN_NAME", "ZUNO_PLUGIN_NAME"),
    (
        "OPENCODE_PLUGIN_PROTOCOL_VERSION",
        "ZUNO_PLUGIN_PROTOCOL_VERSION",
    ),
    ("OPENCODE_PREFIXES", "ZUNO_PREFIXES"),
    ("OPENCODE_PRINT_LOGS", "ZUNO_PRINT_LOGS"),
    ("OPENCODE_PURE", "ZUNO_PURE"),
    ("OPENCODE_RUST_BUILD_ID", "ZUNO_RUST_BUILD_ID"),
    ("OPENCODE_SEARCH_BACKEND", "ZUNO_SEARCH_BACKEND"),
    ("OPENCODE_SHOW_TTFD", "ZUNO_SHOW_TTFD"),
    ("OPENCODE_SKILL_PATTERN", "ZUNO_SKILL_PATTERN"),
    (
        "OPENCODE_STREAM_IDLE_TIMEOUT_SECS",
        "ZUNO_STREAM_IDLE_TIMEOUT_SECS",
    ),
    ("OPENCODE_TERMINAL", "ZUNO_TERMINAL"),
    ("OPENCODE_TEST_HOME", "ZUNO_TEST_HOME"),
    (
        "OPENCODE_TEST_MANAGED_CONFIG_DIR",
        "ZUNO_TEST_MANAGED_CONFIG_DIR",
    ),
    ("OPENCODE_TUI_CONFIG", "ZUNO_TUI_CONFIG"),
    ("OPENCODE_VERSION", "ZUNO_VERSION"),
    ("OPENCODE_WEBSEARCH_PROVIDER", "ZUNO_WEBSEARCH_PROVIDER"),
    ("OPENCODE_WORKSPACE_ID", "ZUNO_WORKSPACE_ID"),
    ("OPENCODE_ZOD_FIXTURE", "ZUNO_ZOD_FIXTURE"),
];

/// Returns the only external spelling accepted for a project environment name.
#[must_use]
pub fn accepted_env_name(name: &str) -> &str {
    if PLUGIN_ABI_ENV_NAMES.contains(&name) {
        return name;
    }
    ZUNO_ENV_NAME_MAP
        .iter()
        .find_map(|(internal, external)| (*internal == name).then_some(*external))
        .unwrap_or(name)
}

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
/// `OPENCODE_CONFIG_DIR`, an extra configuration directory.
pub const OPENCODE_CONFIG_DIR: &str = "OPENCODE_CONFIG_DIR";
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
    /// aborting. See `.omo/notepads/opencode-rust/issues.md`.
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
        self.vars.get(accepted_env_name(key)).map(String::as_str)
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
    fn zuno_names_replace_non_plugin_opencode_names_without_fallback() {
        let zuno = Env::empty().with("ZUNO_DB", "new");
        assert_eq!(zuno.value("OPENCODE_DB"), Some("new"));

        let legacy = Env::empty().with("OPENCODE_DB", "old");
        assert_eq!(legacy.value("OPENCODE_DB"), None);
    }

    #[test]
    fn plugin_abi_names_keep_only_their_opencode_spelling() {
        for name in PLUGIN_ABI_ENV_NAMES {
            let old = Env::empty().with(name, "plugin");
            assert_eq!(old.value(name), Some("plugin"), "{name}");

            let zuno_name = name.replacen("OPENCODE_", "ZUNO_", 1);
            let renamed = Env::empty().with(zuno_name, "renamed");
            assert_eq!(renamed.value(name), None, "{name}");
        }
    }

    #[test]
    fn every_project_owned_name_accepts_only_its_zuno_spelling() {
        let mut internal_names = std::collections::BTreeSet::new();
        let mut external_names = std::collections::BTreeSet::new();

        for (internal, external) in ZUNO_ENV_NAME_MAP {
            assert!(
                internal_names.insert(internal),
                "duplicate internal name {internal}"
            );
            assert!(
                external_names.insert(external),
                "duplicate external name {external}"
            );
            assert!(internal.starts_with("OPENCODE_"), "{internal}");
            assert!(external.starts_with("ZUNO_"), "{external}");
            assert!(!PLUGIN_ABI_ENV_NAMES.contains(&internal), "{internal}");

            let accepted = Env::empty().with(external, "accepted");
            assert_eq!(accepted.value(internal), Some("accepted"), "{internal}");

            let rejected = Env::empty().with(internal, "legacy");
            assert_eq!(rejected.value(internal), None, "{internal}");
        }

        assert_eq!(internal_names.len(), ZUNO_ENV_NAME_MAP.len());
        assert_eq!(external_names.len(), ZUNO_ENV_NAME_MAP.len());
        // A floor, not an equality: the guarded failure is a name *losing* its entry,
        // which would silently accept its `OPENCODE_` spelling again. Adding a name is
        // not a failure, so an equality here would only demand its own maintenance.
        assert!(ZUNO_ENV_NAME_MAP.len() >= 66, "the map only ever grows");
    }
}
