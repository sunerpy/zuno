use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use oc_plugin::SUPPORTED_JS_PLUGINS;

const PLUGIN_CACHE: &str = "/config/.cache/opencode";
const ANTIGRAVITY_PACKAGE: &str = "opencode-antigravity-auth";
const KIRO_PACKAGE: &str = "@sunerpy/opencode-kiro-auth";

fn supported_spec(package: &str) -> String {
    let entry = SUPPORTED_JS_PLUGINS
        .iter()
        .find(|supported| supported.package == package)
        .unwrap_or_else(|| panic!("oc_plugin::SUPPORTED_JS_PLUGINS no longer lists {package}"));
    format!("{}@{}", entry.package, entry.version)
}

fn installed_package(package: &str) -> PathBuf {
    Path::new(PLUGIN_CACHE).join(format!(
        "packages/{}/node_modules/{package}",
        supported_spec(package)
    ))
}

#[tokio::test]
async fn real_auth_plugin_providers_reach_the_plain_models_surface() {
    let absent = [ANTIGRAVITY_PACKAGE, KIRO_PACKAGE]
        .into_iter()
        .map(installed_package)
        .filter(|path| !path.is_dir())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !absent.is_empty() {
        eprintln!(
            "SKIPPED real_auth_plugin_providers_reach_the_plain_models_surface: {} is absent, so \
             the real supported plugins were NOT loaded on this host",
            absent.join(", ")
        );
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let catalog = root.path().join("models.json");
    std::fs::write(
        &catalog,
        r#"{
            "google": {
                "id": "google",
                "name": "Google",
                "npm": "@ai-sdk/google",
                "models": {
                    "gemini-test": { "id": "gemini-test", "name": "Gemini Test" }
                }
            }
        }"#,
    )
    .expect("write pinned models catalog");
    let config = serde_json::json!({
        "plugin": [
            supported_spec(ANTIGRAVITY_PACKAGE),
            supported_spec(KIRO_PACKAGE),
        ],
    });

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencode-rust"));
    command
        .arg("models")
        .current_dir(root.path())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env("HOME", root.path().join("home"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", "/config/.cache")
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("MISE_DATA_DIR", "/config/.local/share/mise")
        .env("PATH", "/usr/bin:/bin")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "true")
        .env("OPENCODE_MODELS_PATH", &catalog)
        .env("OPENCODE_CONFIG_CONTENT", config.to_string())
        .env(
            "OPENCODE_AUTH_CONTENT",
            r#"{"google":{"type":"api","key":"test"},"kiro-auth":{"type":"api","key":"test"}}"#,
        );

    let output = tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("models command timed out")
        .expect("run models command");
    assert!(
        output.status.success(),
        "models failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("models output is UTF-8");
    let providers = stdout
        .lines()
        .filter_map(|line| line.split_once('/').map(|(provider, _)| provider))
        .collect::<BTreeSet<_>>();
    assert!(
        providers.contains("kiro-auth"),
        "loading kiro-auth is insufficient: its contributed models must reach plain `models` \
         output; providers={providers:?}, stdout={stdout:?}"
    );
    assert!(
        providers.contains("google"),
        "the antigravity-backed Google provider regressed; providers={providers:?}, \
         stdout={stdout:?}"
    );
}
