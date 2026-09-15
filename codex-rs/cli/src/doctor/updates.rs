//! Reports the intentionally closed Zuno preview update boundary.
//!
//! Preview releases are promoted from exact sealed candidates, but Zuno does
//! not yet ship a product-owned atomic installer. Doctor therefore performs no
//! release, package-manager, desktop-appcast, or Store version probe. This is a
//! deliberate capability state rather than a degraded network check.

use codex_core::config::Config;

use super::CheckStatus;
use super::DoctorCheck;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::desktop::platform::InstalledApp;

const PREVIEW_UPDATE_SUMMARY: &str =
    "automatic update and version probing are disabled for the Zuno preview";
const PREVIEW_UPDATE_DETAIL: &str =
    "latest version probe: disabled until a Zuno-owned atomic updater passes all platform gates";
const PREVIEW_UPDATE_ACTION: &str = "update action: manual Zuno release download";

/// Builds the update-health row without network or package-manager I/O.
pub(super) async fn updates_check(_config: &Config) -> DoctorCheck {
    preview_update_check()
}

fn preview_update_check() -> DoctorCheck {
    DoctorCheck::new(
        "updates.status",
        "updates",
        CheckStatus::Ok,
        PREVIEW_UPDATE_SUMMARY,
    )
    .details(vec![
        PREVIEW_UPDATE_DETAIL.to_string(),
        PREVIEW_UPDATE_ACTION.to_string(),
    ])
}

/// Official Codex desktop update feeds are unrelated to the independent Zuno
/// preview and must never contribute reachability or update status.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(super) async fn append_desktop_update(
    checks: &mut [DoctorCheck],
    _config: Option<&Config>,
    _application: &InstalledApp,
) {
    if let Some(update) = checks.iter_mut().find(|check| check.id == "updates.status") {
        update
            .details
            .push("official Codex desktop update feeds: disabled for Zuno".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_update_check_is_explicitly_offline_and_non_mutating() {
        let check = preview_update_check();
        assert_eq!(check.status, CheckStatus::Ok);
        assert_eq!(check.summary, PREVIEW_UPDATE_SUMMARY);
        assert_eq!(
            check.details,
            [PREVIEW_UPDATE_DETAIL, PREVIEW_UPDATE_ACTION]
        );
        assert!(check.issues.is_empty());
        assert!(check.remediation.is_none());
    }
}
