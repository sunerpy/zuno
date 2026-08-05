//! Failures from loading or running a plugin.

use crate::recovery::{Recoverable, Recovery};
use crate::source::BoxSource;
use std::time::Duration;

/// A failure from loading or running a plugin.
///
/// Every variant names the plugin, because a plugin failure must never be
/// attributable to the host: a user reading "hook failed" learns nothing, and a
/// host that cannot name the offending plugin cannot disable it.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The plugin could not be located, read, or evaluated.
    #[error("plugin {plugin} failed to load")]
    Load {
        plugin: String,
        #[source]
        source: BoxSource,
    },

    /// A hook the plugin registered raised an error. `hook` identifies which one,
    /// so the host can disable a single hook rather than the whole plugin.
    #[error("plugin {plugin} failed in hook {hook}")]
    Hook {
        plugin: String,
        hook: String,
        #[source]
        source: BoxSource,
    },

    /// A hook exceeded its time budget. Retryable: a plugin host that was slow to
    /// start is the common cause.
    #[error("plugin {plugin} timed out in hook {hook} after {elapsed:?}")]
    Timeout {
        plugin: String,
        hook: String,
        elapsed: Duration,
    },

    /// The plugin declares an API version this build does not implement. Both
    /// versions are carried so the reporter can tell the user what to upgrade.
    #[error("plugin {plugin} requires API version {required}, this build provides {provided}")]
    IncompatibleApi {
        plugin: String,
        required: String,
        provided: String,
    },
}

impl PluginError {
    /// The name of the plugin that failed.
    #[must_use]
    pub fn plugin(&self) -> &str {
        match self {
            Self::Load { plugin, .. }
            | Self::Hook { plugin, .. }
            | Self::Timeout { plugin, .. }
            | Self::IncompatibleApi { plugin, .. } => plugin,
        }
    }

    /// The action this failure calls for.
    #[must_use]
    pub fn recovery(&self) -> Recovery {
        Recoverable::recovery(self)
    }

    /// True when invoking the plugin again may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        Recoverable::recovery(self).is_retry()
    }
}

impl Recoverable for PluginError {
    fn recovery(&self) -> Recovery {
        match self {
            Self::Timeout { .. } => Recovery::Retry { after: None },
            Self::Load { .. } | Self::Hook { .. } | Self::IncompatibleApi { .. } => Recovery::Fail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_variant() -> Vec<PluginError> {
        vec![
            PluginError::Load {
                plugin: "oc-notify".to_owned(),
                source: Box::new(std::io::Error::other("module not found")),
            },
            PluginError::Hook {
                plugin: "oc-notify".to_owned(),
                hook: "tool.execute.before".to_owned(),
                source: Box::new(std::io::Error::other("TypeError")),
            },
            PluginError::Timeout {
                plugin: "oc-notify".to_owned(),
                hook: "tool.execute.before".to_owned(),
                elapsed: Duration::from_secs(5),
            },
            PluginError::IncompatibleApi {
                plugin: "oc-notify".to_owned(),
                required: "2.0".to_owned(),
                provided: "1.4".to_owned(),
            },
        ]
    }

    #[test]
    fn every_variant_names_its_plugin() {
        for e in every_variant() {
            assert_eq!(e.plugin(), "oc-notify", "{e}");
        }
    }

    #[test]
    fn only_timeout_is_retryable() {
        for e in every_variant() {
            let expected = matches!(e, PluginError::Timeout { .. });
            assert_eq!(e.is_retryable(), expected, "{e}");
        }
    }

    #[test]
    fn hook_failure_identifies_the_hook_and_chains_the_cause() {
        use std::error::Error as _;

        let e = PluginError::Hook {
            plugin: "oc-notify".to_owned(),
            hook: "tool.execute.before".to_owned(),
            source: Box::new(std::io::Error::other("TypeError")),
        };
        assert_eq!(
            e.to_string(),
            "plugin oc-notify failed in hook tool.execute.before"
        );
        assert_eq!(
            e.source().map(ToString::to_string).as_deref(),
            Some("TypeError")
        );
    }

    #[test]
    fn incompatible_api_reports_both_versions() {
        let e = PluginError::IncompatibleApi {
            plugin: "oc-notify".to_owned(),
            required: "2.0".to_owned(),
            provided: "1.4".to_owned(),
        };
        assert_eq!(
            e.to_string(),
            "plugin oc-notify requires API version 2.0, this build provides 1.4"
        );
        assert_eq!(e.recovery(), Recovery::Fail);
    }
}
