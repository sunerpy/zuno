//! Startup environment resolution without process-global mutation.
//!
//! Rust 2024 makes `std::env::set_var` unsafe and this workspace forbids unsafe
//! code. The CLI therefore resolves the environment as an [`oc_paths::Env`] value
//! and starts its command process with those overrides. Downstream services see
//! the same real environment upstream's middleware writes, while unit tests can
//! inspect the value without racing another test.

use std::collections::BTreeMap;

use oc_paths::Env;
use oc_tools::exposure::ExposureFlags;

use crate::GlobalOptions;

/// The non-`OPENCODE_*` marker inherited by child agents.
pub const AGENT: &str = "AGENT";
/// Marks a process launched by OpenCode.
pub const OPENCODE: &str = "OPENCODE";
/// Identifies the OpenCode process to child integrations.
pub const OPENCODE_PID: &str = "OPENCODE_PID";
/// Enables the additional stderr log sink.
pub const OPENCODE_PRINT_LOGS: &str = "OPENCODE_PRINT_LOGS";
/// Selects one of the four upstream log levels.
pub const OPENCODE_LOG_LEVEL: &str = "OPENCODE_LOG_LEVEL";
/// Disables external plugins while preserving built-ins.
pub const OPENCODE_PURE: &str = "OPENCODE_PURE";

/// Every `OPENCODE_*` input read by `flag.ts:3-78`, plus the four values produced
/// or consumed by the CLI middleware itself.
///
/// There are 33 unique names in the source extraction and four CLI/runtime names:
/// `OPENCODE`, `OPENCODE_PID`, `OPENCODE_PRINT_LOGS`, and `OPENCODE_LOG_LEVEL`.
pub const OPENCODE_FLAG_NAMES: [&str; 37] = [
    "OPENCODE_ALWAYS_NOTIFY_UPDATE",
    "OPENCODE_AUTO_HEAP_SNAPSHOT",
    "OPENCODE_CLIENT",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG_DIR",
    "OPENCODE_DB",
    "OPENCODE_DISABLE_AUTOCOMPACT",
    "OPENCODE_DISABLE_AUTOUPDATE",
    "OPENCODE_DISABLE_FFF",
    "OPENCODE_DISABLE_MODELS_FETCH",
    "OPENCODE_DISABLE_MOUSE",
    "OPENCODE_DISABLE_PROJECT_CONFIG",
    "OPENCODE_DISABLE_PRUNE",
    "OPENCODE_DISABLE_TERMINAL_TITLE",
    "OPENCODE_EXPERIMENTAL",
    "OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT",
    "OPENCODE_EXPERIMENTAL_DISABLE_FILEWATCHER",
    "OPENCODE_EXPERIMENTAL_FILEWATCHER",
    "OPENCODE_EXPERIMENTAL_REFERENCES",
    "OPENCODE_EXPERIMENTAL_WORKSPACES",
    "OPENCODE_FAKE_VCS",
    "OPENCODE_GIT_BASH_PATH",
    "OPENCODE_MODELS_PATH",
    "OPENCODE_MODELS_URL",
    "OPENCODE_PERMISSION",
    "OPENCODE_PLUGIN_META_FILE",
    OPENCODE_PURE,
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
    "OPENCODE_SHOW_TTFD",
    "OPENCODE_TUI_CONFIG",
    "OPENCODE_WORKSPACE_ID",
    OPENCODE,
    OPENCODE_LOG_LEVEL,
    OPENCODE_PID,
    OPENCODE_PRINT_LOGS,
];

/// The complete flag snapshot handed to command implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeFlags {
    values: BTreeMap<&'static str, Option<String>>,
    /// Existing typed exposure semantics, including the experimental fallback rule.
    pub exposure: ExposureFlags,
}

impl OpenCodeFlags {
    /// Reads all 37 known names from one immutable environment value.
    #[must_use]
    pub fn read(env: &Env) -> Self {
        let values = OPENCODE_FLAG_NAMES
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
            (OPENCODE, "1".to_owned()),
            (OPENCODE_PID, std::process::id().to_string()),
        ]);
        if globals.print_logs {
            overrides.insert(OPENCODE_PRINT_LOGS, "1".to_owned());
        }
        if let Some(level) = globals.log_level {
            overrides.insert(OPENCODE_LOG_LEVEL, level.as_str().to_owned());
        }
        if globals.pure {
            overrides.insert(OPENCODE_PURE, "1".to_owned());
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
        assert_eq!(snapshot.len(), 37);
        assert_eq!(snapshot.iter().count(), OPENCODE_FLAG_NAMES.len());
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
        assert_eq!(startup.flags.value(OPENCODE), Some("1"));
        assert_eq!(
            startup.flags.value(OPENCODE_PID),
            Some(std::process::id().to_string().as_str())
        );
        assert_eq!(startup.flags.value(OPENCODE_PRINT_LOGS), Some("1"));
        assert_eq!(startup.flags.value(OPENCODE_LOG_LEVEL), Some("WARN"));
        assert_eq!(startup.flags.value(OPENCODE_PURE), Some("1"));
    }

    #[test]
    fn unset_cli_options_preserve_existing_environment_values() {
        let base = Env::empty()
            .with(OPENCODE_PRINT_LOGS, "1")
            .with(OPENCODE_LOG_LEVEL, "DEBUG")
            .with(OPENCODE_PURE, "1");
        let startup = StartupEnvironment::resolve(&base, &GlobalOptions::default());

        assert_eq!(startup.flags.value(OPENCODE_PRINT_LOGS), Some("1"));
        assert_eq!(startup.flags.value(OPENCODE_LOG_LEVEL), Some("DEBUG"));
        assert_eq!(startup.flags.value(OPENCODE_PURE), Some("1"));
    }

    #[test]
    fn specific_experimental_false_beats_blanket_true() {
        let env = Env::empty()
            .with("OPENCODE_EXPERIMENTAL", "true")
            .with("OPENCODE_EXPERIMENTAL_PLAN_MODE", "false");
        let snapshot = OpenCodeFlags::read(&env);
        assert!(!snapshot.exposure.experimental_plan_mode);
    }
}
