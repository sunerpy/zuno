use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oc_engine::terminal_lease::{TerminalBroker, TerminalLease};
use oc_paths::ResolvedProject;
use oc_plugin::{
    AuthApiResult, AuthMethod, AuthPrompt, HookInvocation, JsDiagnosticKind, JsHostConfig,
    JsHostPolicy, JsPluginSpec, Plugin, TextCompleteInput, TextCompleteOutput, VersionGate,
    load_js_plugins_ordered,
};
use oc_testkit::FakeTerminalOwner;
use url::Url;

const FIXTURE: &str = r#"
import { existsSync, writeFileSync } from "node:fs";

export default {
  id: "resident-fixture",
  server: async (_input, options) => ({
    auth: {
      provider: "resident-fixture",
      methods: [{
        type: "api",
        label: "Fixture API key",
        prompts: [{
          type: "text",
          key: "key",
          message: "API key",
          validate: (value) => value === "valid" ? undefined : "invalid key",
        }],
        authorize: async () => ({ type: "failed" }),
      }],
    },
    "experimental.text.complete": async (_input, output) => {
      if (options?.hangOnce && !existsSync(options.hangOnce)) {
        writeFileSync(options.hangOnce, "hung");
        await new Promise(() => {});
      }
      output.text += "-resident";
    },
  }),
};
"#;

fn fixture(root: &Path) -> PathBuf {
    let path = root.join("resident-fixture.mjs");
    std::fs::write(&path, FIXTURE).expect("write JavaScript fixture");
    path
}

fn host(root: &Path, terminal: Arc<dyn TerminalLease>, policy: JsHostPolicy) -> JsHostConfig {
    JsHostConfig::new(
        ResolvedProject {
            previous: None,
            id: "fixture-project".to_owned(),
            directory: root.to_path_buf(),
            vcs: None,
        },
        Url::parse("http://127.0.0.1:4096").expect("server URL"),
        terminal,
    )
    .directory(root)
    .worktree(root)
    .cache_dir(root.join("cache"))
    .policy(policy)
}

fn file_spec(path: &Path) -> JsPluginSpec {
    JsPluginSpec::new(format!("file:{}", path.display()))
}

#[tokio::test]
async fn js_missing_runtime_names_every_affected_plugin() {
    // Given
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let config =
        host(temp.path(), terminal, JsHostPolicy::default()).runtime_search_path(OsString::new());
    let specs = vec![
        JsPluginSpec::new("missing-one@1.0.0"),
        JsPluginSpec::new("missing-two@2.0.0"),
    ];

    // When
    let load = load_js_plugins_ordered(specs, config).await;

    // Then
    assert!(load.plugins().is_empty());
    assert_eq!(load.diagnostics().len(), 2);
    assert!(load.diagnostics().iter().all(|diagnostic| {
        diagnostic.kind == JsDiagnosticKind::MissingRuntime
            && diagnostic.message.contains("bun")
            && diagnostic.message.contains("node")
    }));
    assert_eq!(load.diagnostics()[0].plugin, "missing-one@1.0.0");
    assert_eq!(load.diagnostics()[1].plugin, "missing-two@2.0.0");
}

#[tokio::test]
async fn js_prompt_validator_remains_callable_after_module_initialization() {
    // Given
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let load = load_js_plugins_ordered(
        vec![file_spec(&fixture(temp.path()))],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let auth = plugin.auth().expect("fixture auth hook");
    let AuthMethod::Api { prompts, .. } = &auth.methods[0] else {
        panic!("fixture must expose an API method");
    };
    let AuthPrompt::Text {
        validate: Some(validate),
        ..
    } = &prompts[0]
    else {
        panic!("fixture must retain its validator");
    };

    // When / Then
    assert_eq!(validate("wrong").as_deref(), Some("invalid key"));
    assert_eq!(validate("valid"), None);
    load.shutdown().await;
}

#[tokio::test]
async fn js_timed_out_hook_restarts_before_the_next_invocation() {
    // Given
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let marker = temp.path().join("hung-once");
    let policy = JsHostPolicy::default()
        .hook_timeout(Duration::from_millis(80))
        .max_restarts(1);
    let spec = file_spec(&fixture(temp.path())).options(serde_json::json!({
        "hangOnce": marker,
    }));
    let load = load_js_plugins_ordered(vec![spec], host(temp.path(), terminal, policy)).await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let mut first = TextCompleteOutput {
        text: "first".to_owned(),
    };

    // When
    plugin
        .call(&mut HookInvocation::TextComplete {
            input: &TextCompleteInput {
                session_id: "session",
                message_id: "message",
                part_id: "part",
            },
            output: &mut first,
        })
        .await
        .expect("timeout is contained by the host");
    let mut second = TextCompleteOutput {
        text: "second".to_owned(),
    };
    plugin
        .call(&mut HookInvocation::TextComplete {
            input: &TextCompleteInput {
                session_id: "session",
                message_id: "message",
                part_id: "part",
            },
            output: &mut second,
        })
        .await
        .expect("next invocation restarts the runtime");

    // Then
    assert_eq!(first.text, "first");
    assert_eq!(second.text, "second-resident");
    assert_eq!(plugin.restart_count(), 1);
    assert!(
        plugin
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.kind == JsDiagnosticKind::TimedOut)
    );
    load.shutdown().await;
}

#[tokio::test]
async fn js_authorize_holds_and_releases_the_terminal_lease() {
    // Given
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let transcript = owner.transcript();
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let load = load_js_plugins_ordered(
        vec![file_spec(&fixture(temp.path()))],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let auth = plugin.auth().expect("fixture auth hook");
    let AuthMethod::Api {
        authorize: Some(authorize),
        ..
    } = &auth.methods[0]
    else {
        panic!("fixture must expose an API authorizer");
    };

    // When
    let result = authorize.authorize(None).await.expect("authorize call");

    // Then
    assert!(matches!(result, AuthApiResult::Failed));
    assert!(transcript.acquired_by("resident-fixture"));
    assert!(transcript.released_by("resident-fixture"));
    load.shutdown().await;
}

#[tokio::test]
async fn js_memory_ceiling_is_enforced_without_a_hook_call() {
    // Given
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let policy = JsHostPolicy::default().memory_limit_mib(1).max_restarts(0);
    let load = load_js_plugins_ordered(
        vec![file_spec(&fixture(temp.path()))],
        host(temp.path(), terminal, policy),
    )
    .await;

    // When
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Then
    assert!(load.diagnostics().iter().any(|diagnostic| {
        diagnostic.kind == JsDiagnosticKind::Crashed && diagnostic.message.contains("resident set")
    }));
    load.shutdown().await;
}

#[test]
fn js_version_gate_records_an_incompatible_peer_range() {
    // Given
    let temp = tempfile::tempdir().expect("tempdir");
    let package = temp
        .path()
        .join("packages/example@1.0.0/node_modules/example");
    std::fs::create_dir_all(&package).expect("package directory");
    std::fs::write(package.join("index.js"), "export default {};").expect("entrypoint");
    std::fs::write(
        package.join("package.json"),
        r#"{"name":"example","version":"1.0.0","peerDependencies":{"@opencode-ai/plugin":"^0.1.0"}}"#,
    )
    .expect("package manifest");

    // When
    let resolved = JsPluginSpec::new("example@1.0.0")
        .resolve(temp.path())
        .expect("installed package resolves");

    // Then
    assert!(matches!(resolved.gate(), VersionGate::Unsatisfied { .. }));
}

#[tokio::test]
async fn js_real_supported_plugins_load_with_their_own_sdk_clients() {
    // Given
    let cache = PathBuf::from("/config/.cache/opencode");
    let antigravity = cache
        .join("packages/opencode-antigravity-auth@1.6.0/node_modules/opencode-antigravity-auth");
    let kiro = cache.join(
        "packages/@sunerpy/opencode-kiro-auth@0.20.1/node_modules/@sunerpy/opencode-kiro-auth",
    );
    if !antigravity.is_dir() || !kiro.is_dir() {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let config = host(temp.path(), terminal, JsHostPolicy::default()).cache_dir(&cache);

    // When
    let load = load_js_plugins_ordered(
        vec![
            JsPluginSpec::new("opencode-antigravity-auth@1.6.0"),
            JsPluginSpec::new("@sunerpy/opencode-kiro-auth@0.20.1"),
        ],
        config,
    )
    .await;

    // Then
    assert_eq!(load.plugins().len(), 2, "{:?}", load.diagnostics());
    let providers = load
        .plugins()
        .iter()
        .filter_map(|plugin| plugin.auth().map(|auth| auth.provider))
        .collect::<Vec<_>>();
    assert_eq!(providers, ["google", "kiro-auth"]);
    assert!(load.plugins().iter().all(|plugin| {
        plugin
            .init_report()
            .is_some_and(|report| report.sdk.is_some())
    }));
    load.shutdown().await;
}
