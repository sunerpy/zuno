//! Startup environment resolution without process-global mutation.
//!
//! Rust 2024 makes `std::env::set_var` unsafe and this workspace forbids unsafe
//! code. The CLI therefore resolves the environment as an [`zuno_paths::Env`] value
//! and starts its command process with those overrides. Downstream services see
//! the same real environment upstream's middleware writes, while unit tests can
//! inspect the value without racing another test.

use std::collections::BTreeMap;

use zuno_paths::Env;
use zuno_tools::exposure::ExposureFlags;

use crate::GlobalOptions;

/// The non-`OPENCODE_*` marker inherited by child agents.
pub const AGENT: &str = "AGENT";
/// Marks a process launched by OpenCode.
pub const ZUNO: &str = "ZUNO";
/// Identifies the OpenCode process to child integrations.
pub const ZUNO_PID: &str = "ZUNO_PID";
/// Enables the additional stderr log sink.
pub const ZUNO_PRINT_LOGS: &str = "ZUNO_PRINT_LOGS";
/// Selects one of the four upstream log levels.
pub const ZUNO_LOG_LEVEL: &str = "ZUNO_LOG_LEVEL";
/// Disables external plugins while preserving built-ins.
pub const ZUNO_PURE: &str = "ZUNO_PURE";
/// Starts the JavaScript plugin host, which is otherwise off.
pub const ZUNO_ENABLE_JS_PLUGINS: &str = "ZUNO_ENABLE_JS_PLUGINS";

/// Every `OPENCODE_*` input read by `flag.ts:3-78`, plus the four values produced
/// or consumed by the CLI middleware itself.
///
/// There are 33 unique names in the source extraction and four CLI/runtime names:
/// `OPENCODE`, `OPENCODE_PID`, `OPENCODE_PRINT_LOGS`, and `OPENCODE_LOG_LEVEL`.
pub const ZUNO_FLAG_NAMES: [&str; 39] = [
    "ZUNO_ALWAYS_NOTIFY_UPDATE",
    "ZUNO_AUTO_HEAP_SNAPSHOT",
    "OPENCODE_CLIENT",
    "ZUNO_CONFIG",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG_DIR",
    "ZUNO_DB",
    "ZUNO_DISABLE_AUTOCOMPACT",
    "ZUNO_DISABLE_AUTOUPDATE",
    "OPENCODE_DISABLE_CLAUDE_CODE",
    "ZUNO_DISABLE_FFF",
    "ZUNO_DISABLE_MODELS_FETCH",
    "ZUNO_DISABLE_MOUSE",
    "ZUNO_DISABLE_PROJECT_CONFIG",
    "ZUNO_DISABLE_PRUNE",
    "ZUNO_DISABLE_TERMINAL_TITLE",
    ZUNO_ENABLE_JS_PLUGINS,
    "ZUNO_EXPERIMENTAL",
    "ZUNO_EXPERIMENTAL_DISABLE_COPY_ON_SELECT",
    "ZUNO_EXPERIMENTAL_DISABLE_FILEWATCHER",
    "ZUNO_EXPERIMENTAL_FILEWATCHER",
    "ZUNO_EXPERIMENTAL_REFERENCES",
    "ZUNO_EXPERIMENTAL_WORKSPACES",
    "ZUNO_FAKE_VCS",
    "ZUNO_GIT_BASH_PATH",
    "ZUNO_MODELS_PATH",
    "ZUNO_MODELS_URL",
    "ZUNO_PERMISSION",
    "ZUNO_PLUGIN_META_FILE",
    ZUNO_PURE,
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
    "ZUNO_SHOW_TTFD",
    "ZUNO_TUI_CONFIG",
    "ZUNO_WORKSPACE_ID",
    ZUNO,
    ZUNO_LOG_LEVEL,
    ZUNO_PID,
    ZUNO_PRINT_LOGS,
];

/// The complete flag snapshot handed to command implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeFlags {
    values: BTreeMap<&'static str, Option<String>>,
    /// Existing typed exposure semantics, including the experimental fallback rule.
    pub exposure: ExposureFlags,
}

impl OpenCodeFlags {
    /// Reads every known name from one immutable environment value.
    #[must_use]
    pub fn read(env: &Env) -> Self {
        let values = ZUNO_FLAG_NAMES
            .into_iter()
            .map(|name| (name, env.value(name).map(str::to_owned)))
            .collect();
        let exposure = ExposureFlags::from_lookup(|name| env.value(name).map(str::to_owned));
        Self { values, exposure }
    }

    /// The raw value, preserving set-but-empty separately from absence.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(Option::as_deref)
    }

    /// Every known name and the value observed for it, in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Option<&str>)> {
        self.values
            .iter()
            .map(|(name, value)| (*name, value.as_deref()))
    }

    /// Number of variables read, including absent variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no names are represented. Always false for a valid snapshot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The environment command handlers run under and the flags resolved from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupEnvironment {
    resolved: Env,
    overrides: BTreeMap<&'static str, String>,
    /// All supported `OPENCODE_*` values after CLI precedence is applied.
    pub flags: OpenCodeFlags,
}

impl StartupEnvironment {
    /// Applies CLI middleware precedence to a process snapshot.
    #[must_use]
    pub fn resolve(base: &Env, globals: &GlobalOptions) -> Self {
        let mut overrides = BTreeMap::from([
            (AGENT, "1".to_owned()),
            (ZUNO, "1".to_owned()),
            (ZUNO_PID, std::process::id().to_string()),
        ]);
        if globals.print_logs {
            overrides.insert(ZUNO_PRINT_LOGS, "1".to_owned());
        }
        if let Some(level) = globals.log_level {
            overrides.insert(ZUNO_LOG_LEVEL, level.as_str().to_owned());
        }
        if globals.pure {
            overrides.insert(ZUNO_PURE, "1".to_owned());
        }

        let resolved = overrides.iter().fold(base.clone(), |env, (name, value)| {
            env.with(*name, value.clone())
        });
        let flags = OpenCodeFlags::read(&resolved);
        Self {
            resolved,
            overrides,
            flags,
        }
    }

    /// The complete environment value command implementations should consult.
    #[must_use]
    pub fn resolved(&self) -> &Env {
        &self.resolved
    }

    /// Only the values this CLI must overlay when starting its command process.
    pub fn overrides(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.overrides
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CliLogLevel;

    #[test]
    fn snapshot_reads_every_known_opencode_name_even_when_absent() {
        let snapshot = OpenCodeFlags::read(&Env::empty());
        assert_eq!(snapshot.len(), ZUNO_FLAG_NAMES.len());
        assert_eq!(snapshot.iter().count(), ZUNO_FLAG_NAMES.len());
        assert!(!snapshot.is_empty());
        assert!(snapshot.iter().all(|(_, value)| value.is_none()));
    }

    #[test]
    fn globals_and_required_process_markers_are_applied_before_flags_are_read() {
        let globals = GlobalOptions {
            print_logs: true,
            log_level: Some(CliLogLevel::Warn),
            pure: true,
        };
        let startup = StartupEnvironment::resolve(&Env::empty(), &globals);

        assert_eq!(startup.resolved().value(AGENT), Some("1"));
        assert_eq!(startup.flags.value(ZUNO), Some("1"));
        assert_eq!(
            startup.flags.value(ZUNO_PID),
            Some(std::process::id().to_string().as_str())
        );
        assert_eq!(startup.flags.value(ZUNO_PRINT_LOGS), Some("1"));
        assert_eq!(startup.flags.value(ZUNO_LOG_LEVEL), Some("WARN"));
        assert_eq!(startup.flags.value(ZUNO_PURE), Some("1"));
    }

    #[test]
    fn unset_cli_options_preserve_existing_environment_values() {
        let base = Env::empty()
            .with(ZUNO_PRINT_LOGS, "1")
            .with(ZUNO_LOG_LEVEL, "DEBUG")
            .with(ZUNO_PURE, "1");
        let startup = StartupEnvironment::resolve(&base, &GlobalOptions::default());

        assert_eq!(startup.flags.value(ZUNO_PRINT_LOGS), Some("1"));
        assert_eq!(startup.flags.value(ZUNO_LOG_LEVEL), Some("DEBUG"));
        assert_eq!(startup.flags.value(ZUNO_PURE), Some("1"));
    }

    #[test]
    fn specific_experimental_false_beats_blanket_true() {
        let env = Env::empty()
            .with("ZUNO_EXPERIMENTAL", "true")
            .with("ZUNO_EXPERIMENTAL_PLAN_MODE", "false");
        let snapshot = OpenCodeFlags::read(&env);
        assert!(!snapshot.exposure.experimental_plan_mode);
    }

    #[test]
    fn zuno_owned_names_are_accepted_and_legacy_spellings_are_rejected() {
        let env = Env::empty()
            .with("ZUNO_PURE", "zuno")
            .with("OPENCODE_PURE", "legacy")
            .with("ZUNO_EXPERIMENTAL", "true")
            .with("OPENCODE_EXPERIMENTAL_PLAN_MODE", "false");
        let snapshot = OpenCodeFlags::read(&env);

        assert_eq!(snapshot.value("ZUNO_PURE"), Some("zuno"));
        assert_eq!(snapshot.value("OPENCODE_PURE"), None);
        assert!(snapshot.exposure.experimental_plan_mode);
    }

    #[test]
    fn plugin_abi_names_keep_their_opencode_spelling() {
        let env = Env::from_pairs(
            zuno_paths::env::PLUGIN_ABI_ENV_NAMES
                .into_iter()
                .map(|name| (name, format!("value-for-{name}"))),
        );
        let snapshot = OpenCodeFlags::read(&env);

        for name in zuno_paths::env::PLUGIN_ABI_ENV_NAMES {
            assert_eq!(
                snapshot.value(name),
                env.value(name),
                "plugin ABI variable {name} must remain visible"
            );
        }
    }
}
