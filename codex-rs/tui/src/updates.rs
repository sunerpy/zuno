#![cfg(not(debug_assertions))]

use crate::legacy_core::config::Config;

pub(crate) use crate::updates_cache::dismiss_version;

/// Zuno preview releases never perform a background version probe.
///
/// The preview release is promoted from an exact sealed candidate and has no
/// product-owned atomic updater yet. Reading GitHub's floating `latest` route
/// would either see the legacy Zuno line or hide the draft/prerelease candidate,
/// and would create update UI that cannot safely apply the result. Keep the
/// complete TUI update path closed until the installer and daemon share one
/// verified Zuno package-selection contract.
pub fn get_upgrade_version(_config: &Config) -> Option<String> {
    None
}

/// No preview update modal is shown because there is no executable update action.
pub fn get_upgrade_version_for_popup(_config: &Config) -> Option<String> {
    None
}
