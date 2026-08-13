//! What the two real supported auth plugins contribute to a user-visible surface.
//!
//! The two plugins reach the user through **different** hooks, and this file keeps
//! them apart because conflating them is what let a green test assert something it
//! never measured:
//!
//! * kiro-auth registers a `config` hook that inserts provider `kiro-auth` plus a
//!   `provider` hook that supplies its models, so its contribution lands in plain
//!   `models` stdout and is provable there.
//! * antigravity registers `auth`, `tool` and `event` hooks — and **no** `config`
//!   or `provider` hook. `models` dispatches only `config` and `provider`
//!   (`crates/oc-cli/src/cmd/models.rs:92-127`), so antigravity contributes nothing
//!   to that surface. Measured on this host: `models --verbose` stdout is
//!   byte-identical with and without antigravity in the plugin list (2,944 lines
//!   each, `diff` empty).
//!
//! The pinned catalog fixture below already declares provider `google` with a
//! `gemini-test` model, so asserting "provider `google` appears" cannot distinguish
//! antigravity from the fixture — removing antigravity used to leave that assertion
//! green. Antigravity's contribution is therefore proven where antigravity actually
//! acts: the auth resource it registers for provider `google`, carrying a method
//! label only its own code can supply.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use oc_engine::terminal_lease::{
    LeaseReason, TerminalLease, TerminalLeaseError, TerminalLeaseGuard,
};
use oc_plugin::{
    AuthHook, AuthMethod, HookBus, HookInvocation, JsHostConfig, JsPluginSpec, Plugin,
    SUPPORTED_JS_PLUGINS, load_js_plugins_ordered,
};

const PLUGIN_CACHE: &str = "/config/.cache/opencode";
const ANTIGRAVITY_PACKAGE: &str = "opencode-antigravity-auth";
const KIRO_PACKAGE: &str = "@sunerpy/opencode-kiro-auth";

/// The provider id antigravity registers its auth resource for.
///
/// Deliberately the same id the pinned catalog already declares, because that
/// collision is the whole reason the id alone proves nothing.
const ANTIGRAVITY_PROVIDER: &str = "google";

/// A string only antigravity's own code contains (`dist/src/plugin.js:1965`).
///
/// Neither the pinned catalog nor the auth fixture nor kiro-auth can produce it,
/// which is exactly what makes it usable as antigravity-specific evidence.
const ANTIGRAVITY_OAUTH_LABEL: &str = "OAuth with Google (Antigravity)";

/// The catalog every test here pins, so no test depends on a fetched models.dev.
const PINNED_CATALOG: &str = r#"{
    "google": {
        "id": "google",
        "name": "Google",
        "npm": "@ai-sdk/google",
        "models": {
            "gemini-test": {
                "id": "gemini-test",
                "name": "Gemini Test",
                "cost": { "input": 1.25, "output": 5.0 }
            }
        }
    }
}"#;

const AUTH_FAILURE_CATALOG: &str = r#"{
    "test": {
        "id": "test",
        "name": "Test",
        "npm": "@ai-sdk/openai-compatible",
        "models": {
            "test-model": {
                "id": "test-model",
                "name": "Test Model",
                "limit": { "context": 100000, "output": 4096 }
            }
        }
    }
}"#;

const FAILING_AUTH_LOADER_PLUGIN: &str = r#"
export default {
  id: "models-failing-auth-loader",
  server: async () => ({
    auth: {
      provider: "test",
      loader: async (getAuth) => {
        await getAuth();
        throw new Error("task173 models auth loader failure");
      },
      methods: [],
    },
  }),
};
"#;

fn supported_spec(package: &str) -> String {
    let entry = SUPPORTED_JS_PLUGINS
        .iter()
        .find(|supported| supported.package == package)
        .unwrap_or_else(|| panic!("oc_plugin::SUPPORTED_JS_PLUGINS no longer lists {package}"));
    format!("{}@{}", entry.package, entry.version)
}

/// The plugin list a production run of this host loads.
///
/// One definition for every test in this file, so dropping a plugin from the
/// product is a single edit that some assertion here must notice.
fn production_plugin_specs() -> Vec<String> {
    vec![
        supported_spec(ANTIGRAVITY_PACKAGE),
        supported_spec(KIRO_PACKAGE),
    ]
}

fn installed_package(package: &str) -> PathBuf {
    Path::new(PLUGIN_CACHE).join(format!(
        "packages/{}/node_modules/{package}",
        supported_spec(package)
    ))
}

/// Names the absent packages so an environment gap cannot read as coverage.
fn absent_packages() -> Vec<String> {
    [ANTIGRAVITY_PACKAGE, KIRO_PACKAGE]
        .into_iter()
        .map(installed_package)
        .filter(|path| !path.is_dir())
        .map(|path| path.display().to_string())
        .collect()
}

fn skipped(test: &str, absent: &[String]) {
    eprintln!(
        "SKIPPED {test}: {} is absent, so the real supported plugins were NOT loaded on this host",
        absent.join(", ")
    );
}

/// Collects the auth resources the named plugins register, through the real loader.
///
/// This is the production path — [`load_js_plugins_ordered`] with the same host
/// configuration `models` builds, then a [`HookBus`] dispatch — rather than a
/// hand-built descriptor, so the evidence has to survive module import, factory
/// invocation and the JS bridge.
async fn auth_hooks_of(specs: Vec<String>) -> Vec<AuthHook> {
    let root = tempfile::tempdir().expect("tempdir");
    let project = oc_paths::project::resolve_project(root.path());
    let terminal: Arc<dyn TerminalLease> = Arc::new(HeadlessTerminalLease);
    let host = JsHostConfig::new(
        project,
        reqwest::Url::parse("http://127.0.0.1:0").expect("static plugin server URL"),
        terminal,
    )
    .directory(root.path())
    .worktree(root.path())
    .cache_dir(PLUGIN_CACHE);

    let expected = specs.len();
    let load =
        load_js_plugins_ordered(specs.into_iter().map(JsPluginSpec::new).collect(), host).await;
    assert_eq!(
        load.plugins().len(),
        expected,
        "every named plugin must load for its hooks to mean anything: {:?}",
        load.diagnostics()
    );
    let plugins = load
        .plugins()
        .iter()
        .cloned()
        .map(|plugin| plugin as Arc<dyn Plugin>)
        .collect();
    let mut hooks = Vec::new();
    HookBus::new(plugins)
        .dispatch(HookInvocation::Auth { output: &mut hooks })
        .await
        .expect("collecting auth hooks from the real plugins");
    load.shutdown().await;
    hooks
}

fn method_labels(hook: &AuthHook) -> Vec<&str> {
    hook.methods
        .iter()
        .map(|method| match method {
            AuthMethod::OAuth { label, .. } | AuthMethod::Api { label, .. } => label.as_str(),
        })
        .collect()
}

/// kiro-auth's contributed models reach plain `models` stdout.
///
/// The `google` row is asserted too, but only as a **fixture control**: it comes
/// from [`PINNED_CATALOG`] plus the auth entry below, and stays green with
/// antigravity removed. Antigravity's own contribution is proven by
/// [`the_real_antigravity_plugin_registers_a_google_auth_method_no_fixture_supplies`].
#[tokio::test]
async fn real_auth_plugin_providers_reach_the_plain_models_surface() {
    let absent = absent_packages();
    if !absent.is_empty() {
        skipped(
            "real_auth_plugin_providers_reach_the_plain_models_surface",
            &absent,
        );
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let catalog = root.path().join("models.json");
    std::fs::write(&catalog, PINNED_CATALOG).expect("write pinned models catalog");
    let config = serde_json::json!({ "plugin": production_plugin_specs() });

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
        providers.contains(ANTIGRAVITY_PROVIDER),
        "the pinned catalog's own `{ANTIGRAVITY_PROVIDER}` row must survive plugin loading; this \
         is a fixture control and NOT evidence about antigravity, which registers no config or \
         provider hook; providers={providers:?}, stdout={stdout:?}"
    );
}

/// Antigravity's auth loader mutates the provider seen by the real `models` command.
///
/// The non-zero fixture price is the control: only the loader can turn it into the
/// free model the released binary reports. Merely loading or dispatching the auth
/// resource is insufficient, so this guards the production loader call itself.
#[tokio::test]
async fn antigravity_auth_loader_zeroes_google_cost_on_the_verbose_models_surface() {
    let absent = absent_packages();
    if !absent.is_empty() {
        skipped(
            "antigravity_auth_loader_zeroes_google_cost_on_the_verbose_models_surface",
            &absent,
        );
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let catalog = root.path().join("models.json");
    std::fs::write(&catalog, PINNED_CATALOG).expect("write pinned models catalog");
    let config = serde_json::json!({
        "plugin": [supported_spec(ANTIGRAVITY_PACKAGE)],
        "provider": { "google": {} }
    });

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencode-rust"));
    command
        .args(["models", "google", "--verbose"])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env_clear()
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
            r#"{"google":{"type":"oauth","refresh":"fixture-refresh","access":"fixture-access","expires":4102444800000}}"#,
        );

    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .expect("verbose models command timed out")
        .expect("run verbose models command");
    assert!(
        output.status.success(),
        "verbose models failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("models output is UTF-8");
    let (_, json) = stdout
        .split_once('\n')
        .unwrap_or_else(|| panic!("verbose output omitted the model JSON: {stdout:?}"));
    let model: serde_json::Value = serde_json::from_str(json)
        .unwrap_or_else(|error| panic!("invalid verbose JSON: {error}\n{json}"));
    assert_eq!(model["id"], "gemini-test");
    assert_eq!(
        model["cost"],
        serde_json::json!({
            "input": 0.0,
            "output": 0.0,
            "cache": { "read": 0.0, "write": 0.0 }
        }),
        "the fixture starts at input=1.25/output=5.0, so only antigravity's real auth loader can produce this cost; stdout={stdout:?}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn failing_auth_loader_is_disabled_and_models_lists_models_with_a_diagnostic() {
    let root = tempfile::tempdir().expect("tempdir");
    let catalog = root.path().join("models.json");
    let plugin = root.path().join("failing-auth-loader.mjs");
    std::fs::write(&catalog, AUTH_FAILURE_CATALOG).expect("write pinned models catalog");
    std::fs::write(&plugin, FAILING_AUTH_LOADER_PLUGIN).expect("write failing auth loader");
    let config = serde_json::json!({
        "plugin": [[format!("file:{}", plugin.display()), {}]],
        "provider": { "test": {} }
    });

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_opencode-rust"));
    command
        .args(["models", "test"])
        .current_dir(root.path())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env_clear()
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
            r#"{"test":{"type":"api","key":"fixture-key"}}"#,
        );

    let output = tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .expect("models command timed out")
        .expect("run models command");
    assert!(
        output.status.success(),
        "the auth loader failure must disable only the plugin, not `models`: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "test/test-model\n",
        "models must remain useful after auth failure"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("models-failing-auth-loader")
            && stderr.contains("auth.loader")
            && stderr.contains("task173 models auth loader failure"),
        "the default models diagnostic must name plugin, hook, and cause: {stderr}"
    );
}

/// Antigravity's contribution, proven with a string no fixture in this file supplies.
#[tokio::test]
async fn the_real_antigravity_plugin_registers_a_google_auth_method_no_fixture_supplies() {
    let absent = absent_packages();
    if !absent.is_empty() {
        skipped(
            "the_real_antigravity_plugin_registers_a_google_auth_method_no_fixture_supplies",
            &absent,
        );
        return;
    }
    assert!(
        !PINNED_CATALOG.contains(ANTIGRAVITY_OAUTH_LABEL),
        "the evidence string must be one the pinned catalog cannot supply, or this test repeats \
         the defect it exists to close"
    );

    let hooks = tokio::time::timeout(
        Duration::from_secs(120),
        auth_hooks_of(production_plugin_specs()),
    )
    .await
    .expect("loading the real plugins timed out");

    let providers = hooks
        .iter()
        .map(|hook| hook.provider.as_str())
        .collect::<Vec<_>>();
    let google = hooks
        .iter()
        .find(|hook| hook.provider == ANTIGRAVITY_PROVIDER)
        .unwrap_or_else(|| {
            panic!(
                "antigravity must register an auth resource for provider \
                 `{ANTIGRAVITY_PROVIDER}`; providers={providers:?}"
            )
        });
    let labels = method_labels(google);
    assert!(
        labels.contains(&ANTIGRAVITY_OAUTH_LABEL),
        "provider `{ANTIGRAVITY_PROVIDER}`'s auth methods must carry antigravity's own label \
         {ANTIGRAVITY_OAUTH_LABEL:?}, which is the part no catalog or auth fixture can fake; \
         labels={labels:?}"
    );
}

/// Negative control: without antigravity, the antigravity-specific evidence is gone.
///
/// The kiro assertion keeps this from passing vacuously — a loader that produced no
/// hooks at all would otherwise "prove" absence.
#[tokio::test]
async fn without_antigravity_no_auth_resource_carries_its_evidence() {
    let absent = absent_packages();
    if !absent.is_empty() {
        skipped(
            "without_antigravity_no_auth_resource_carries_its_evidence",
            &absent,
        );
        return;
    }

    let hooks = tokio::time::timeout(
        Duration::from_secs(120),
        auth_hooks_of(vec![supported_spec(KIRO_PACKAGE)]),
    )
    .await
    .expect("loading the real plugins timed out");

    let providers = hooks
        .iter()
        .map(|hook| hook.provider.as_str())
        .collect::<Vec<_>>();
    assert!(
        providers.contains(&"kiro-auth"),
        "the control must still load a plugin, or it proves absence by loading nothing; \
         providers={providers:?}"
    );
    assert!(
        !providers.contains(&ANTIGRAVITY_PROVIDER),
        "provider `{ANTIGRAVITY_PROVIDER}`'s auth resource must come from antigravity alone; \
         providers={providers:?}"
    );
    let labels = hooks.iter().flat_map(method_labels).collect::<Vec<_>>();
    assert!(
        !labels.contains(&ANTIGRAVITY_OAUTH_LABEL),
        "no other plugin may advertise {ANTIGRAVITY_OAUTH_LABEL:?}; labels={labels:?}"
    );
}

struct HeadlessTerminalLease;

#[async_trait]
impl TerminalLease for HeadlessTerminalLease {
    async fn acquire(&self, reason: LeaseReason) -> Result<TerminalLeaseGuard, TerminalLeaseError> {
        Err(TerminalLeaseError::Unavailable {
            requested_by: reason.plugin,
            detail: "this test cannot host an interactive plugin prompt".to_owned(),
        })
    }
}
