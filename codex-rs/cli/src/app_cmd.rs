use clap::Parser;
use std::path::PathBuf;

const ZUNO_DESKTOP_APP_DISABLED: &str = "Zuno does not launch, download, or install the inherited Codex Desktop app; this compatibility command is disabled";

#[derive(Debug, Parser)]
pub struct AppCommand {
    /// Workspace path retained for command-line compatibility while app launch is disabled.
    #[arg(value_name = "PATH", default_value = ".")]
    pub path: PathBuf,

    /// Installer URL retained for compatibility; Zuno never downloads from it.
    #[arg(long = "download-url")]
    pub download_url_override: Option<String>,
}

pub async fn run_app(cmd: AppCommand) -> anyhow::Result<()> {
    let _ = cmd;
    anyhow::bail!(ZUNO_DESKTOP_APP_DISABLED)
}

#[cfg(test)]
mod tests {
    use super::AppCommand;
    use super::ZUNO_DESKTOP_APP_DISABLED;
    use super::run_app;

    #[tokio::test]
    async fn zuno_app_refuses_paths_and_download_urls_before_side_effects() {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let missing = temp.path().join("workspace-that-must-not-be-created");

        let plain_error = run_app(AppCommand {
            path: missing.clone(),
            download_url_override: None,
        })
        .await
        .expect_err("Zuno app must be disabled");
        let override_error = run_app(AppCommand {
            path: missing.clone(),
            download_url_override: Some("https://example.invalid/Codex.dmg".to_string()),
        })
        .await
        .expect_err("download URL overrides must also be disabled");

        assert_eq!(plain_error.to_string(), ZUNO_DESKTOP_APP_DISABLED);
        assert_eq!(override_error.to_string(), ZUNO_DESKTOP_APP_DISABLED);
        assert!(!missing.exists(), "the refused path must not be created");
    }
}
