//! ACP process-local runtime capacity and lifecycle settings.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

/// Default number of durable ACP sessions one process may keep open.
pub const DEFAULT_ACP_MAX_OPEN_SESSIONS: u32 = 32;
/// Default number of resource-bearing ACP runtimes one process may keep active.
pub const DEFAULT_ACP_MAX_ACTIVE_RUNTIMES: u32 = 8;
/// Default idle period before an eligible ACP runtime may sleep.
pub const DEFAULT_ACP_IDLE_TIMEOUT_MS: u64 = 900_000;
/// Default time an ACP activation waits for runtime capacity.
pub const DEFAULT_ACP_ACTIVATION_WAIT_TIMEOUT_MS: u64 = 30_000;

/// Configuration owned by the ACP client surface.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpConfig {
    /// Process-local session runtime capacity and lifecycle settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<AcpRuntimeConfig>,
}

impl AcpConfig {
    /// Resolve ACP runtime settings with native defaults.
    #[must_use]
    pub fn resolved_runtime(&self) -> ResolvedAcpRuntimeConfig {
        self.runtime.as_ref().map_or_else(
            ResolvedAcpRuntimeConfig::default,
            AcpRuntimeConfig::resolved,
        )
    }
}

/// Optional per-layer ACP runtime overrides.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpRuntimeConfig {
    /// Maximum durable ACP sessions retained by one process. Defaults to 32.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_open_sessions: Option<NonZeroU32>,
    /// Maximum resource-bearing ACP runtimes active at once. Defaults to 8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_active_runtimes: Option<NonZeroU32>,
    /// Idle milliseconds before an eligible ACP runtime may sleep. Defaults to 900000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<NonZeroU64>,
    /// Milliseconds an activation waits for runtime capacity. Defaults to 30000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation_wait_timeout_ms: Option<NonZeroU64>,
}

impl AcpRuntimeConfig {
    /// Fill absent values with the native ACP runtime defaults.
    #[must_use]
    pub fn resolved(&self) -> ResolvedAcpRuntimeConfig {
        let defaults = ResolvedAcpRuntimeConfig::default();
        ResolvedAcpRuntimeConfig {
            max_open_sessions: self
                .max_open_sessions
                .map_or(defaults.max_open_sessions, NonZeroU32::get),
            max_active_runtimes: self
                .max_active_runtimes
                .map_or(defaults.max_active_runtimes, NonZeroU32::get),
            idle_timeout_ms: self
                .idle_timeout_ms
                .map_or(defaults.idle_timeout_ms, NonZeroU64::get),
            activation_wait_timeout_ms: self
                .activation_wait_timeout_ms
                .map_or(defaults.activation_wait_timeout_ms, NonZeroU64::get),
        }
    }
}

/// Fully defaulted ACP runtime settings consumed by the CLI composition root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAcpRuntimeConfig {
    pub max_open_sessions: u32,
    pub max_active_runtimes: u32,
    pub idle_timeout_ms: u64,
    pub activation_wait_timeout_ms: u64,
}

impl ResolvedAcpRuntimeConfig {
    /// Idle period before an eligible runtime may sleep.
    #[must_use]
    pub const fn idle_timeout(self) -> Duration {
        Duration::from_millis(self.idle_timeout_ms)
    }

    /// Maximum wait for active-runtime capacity.
    #[must_use]
    pub const fn activation_wait_timeout(self) -> Duration {
        Duration::from_millis(self.activation_wait_timeout_ms)
    }
}

impl Default for ResolvedAcpRuntimeConfig {
    fn default() -> Self {
        Self {
            max_open_sessions: DEFAULT_ACP_MAX_OPEN_SESSIONS,
            max_active_runtimes: DEFAULT_ACP_MAX_ACTIVE_RUNTIMES,
            idle_timeout_ms: DEFAULT_ACP_IDLE_TIMEOUT_MS,
            activation_wait_timeout_ms: DEFAULT_ACP_ACTIVATION_WAIT_TIMEOUT_MS,
        }
    }
}
