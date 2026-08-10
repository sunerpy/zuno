use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oc_engine::terminal_lease::{TerminalBroker, TerminalLease};
use oc_llm::catalog::availability::Availability;
use oc_llm::catalog::models_dev::CatalogStatus;
use oc_llm::catalog::resolved::{
    ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel, ResolvedProvider,
};
use oc_llm::event::{Message, Role};
use oc_paths::ResolvedProject;
use oc_plugin::{
    AuthApiResult, AuthMethod, AuthPrompt, ChatContext, ChatHeadersOutput, HookInvocation,
    JsDiagnosticKind, JsHostConfig, JsHostPolicy, JsPluginSpec, Plugin, ProviderContext,
    ProviderSource, SUPPORTED_JS_PLUGINS, TextCompleteInput, TextCompleteOutput, VersionGate,
    load_js_plugins_ordered,
};
use oc_testkit::FakeTerminalOwner;
use url::Url;

/// The provider id the kiro plugin registers, from `dist/plugin.js`'s `KIRO_PROVIDER_ID`.
const KIRO_PROVIDER: &str = "kiro-auth";

/// The header kiro injects for a compaction turn, from `dist/core/request/request-kind.js:2`.
const KIRO_REQUEST_KIND_HEADER: &str = "x-opencode-kiro-request-kind";

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

const PLUGIN_CACHE: &str = "/config/.cache/opencode";
const KIRO_PACKAGE: &str = "@sunerpy/opencode-kiro-auth";
const ANTIGRAVITY_PACKAGE: &str = "opencode-antigravity-auth";

/// The `package@version` spec for `package`, taken from the recorded support table.
///
/// Derived rather than typed so this file cannot name a version
/// [`SUPPORTED_JS_PLUGINS`] does not — the exact drift success criterion 6 was
/// narrowed to remove, where the plan, the capture and the test each named a
/// different kiro-auth release and one of them no longer existed on disk.
fn supported_spec(package: &str) -> String {
    let entry = SUPPORTED_JS_PLUGINS
        .iter()
        .find(|supported| supported.package == package)
        .unwrap_or_else(|| panic!("oc_plugin::SUPPORTED_JS_PLUGINS no longer lists {package}"));
    format!("{}@{}", entry.package, entry.version)
}

fn installed_package(cache: &Path, package: &str) -> PathBuf {
    cache.join(format!(
        "packages/{}/node_modules/{package}",
        supported_spec(package)
    ))
}

#[tokio::test]
async fn js_real_supported_plugins_load_with_their_own_sdk_clients() {
    // Given
    let cache = PathBuf::from(PLUGIN_CACHE);
    let antigravity = installed_package(&cache, ANTIGRAVITY_PACKAGE);
    let kiro = installed_package(&cache, KIRO_PACKAGE);
    let absent = [&antigravity, &kiro]
        .into_iter()
        .filter(|path| !path.is_dir())
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    if !absent.is_empty() {
        eprintln!(
            "SKIPPED js_real_supported_plugins_load_with_their_own_sdk_clients: {} is absent, so \
             the real supported plugins were NOT loaded on this host",
            absent.join(", ")
        );
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let config = host(temp.path(), terminal, JsHostPolicy::default()).cache_dir(&cache);

    // When
    let load = load_js_plugins_ordered(
        vec![
            JsPluginSpec::new(supported_spec(ANTIGRAVITY_PACKAGE)),
            JsPluginSpec::new(supported_spec(KIRO_PACKAGE)),
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

/// Success criterion 6's narrowing: one kiro-auth version, in all three places.
///
/// The criterion previously named three mutually inconsistent versions — the plan
/// said `0.18.0`, the capture and the executable test said `0.20.1`, and the user's
/// own `opencode.json` pinned `0.20.6` — and `0.20.1` was no longer installed at
/// all, so the capture and test referenced something that could not load. This pins
/// the convergence: whatever [`SUPPORTED_JS_PLUGINS`] records must be the version
/// the user's config pins, must be the only kiro-auth version any source file or
/// document names, and must actually be resolvable on disk.
#[test]
fn criterion_6_converges_the_plan_the_capture_and_this_test_on_one_kiro_auth_version() {
    let spec = supported_spec(KIRO_PACKAGE);
    let version = spec
        .rsplit_once('@')
        .map(|(_, version)| version.to_owned())
        .unwrap_or_else(|| panic!("{spec} is not a package@version spec"));
    let root = oc_testkit::subject::workspace_root().expect("workspace root");

    let capture = std::fs::read_to_string(root.join("docs/v1-surface-capture.md"))
        .expect("read docs/v1-surface-capture.md");
    assert!(
        capture.contains(&spec),
        "docs/v1-surface-capture.md does not name {spec}; the capture must record the version \
         this host actually loads, or the plugin-route evidence in it was gathered from a \
         different package"
    );

    let plan =
        std::fs::read_to_string(root.join(".omo/plans/opencode-rust.md")).expect("read the plan");
    assert!(
        plan.contains(&format!("(`{version}`)")),
        "the plan's criterion 6 must name `{version}` as the converged version; the whole point \
         of the narrowing was that the plan, the capture and this test stop naming different ones"
    );

    for (label, text) in [
        ("docs/v1-surface-capture.md", capture.as_str()),
        (
            "crates/oc-server/src/compat_v1.rs",
            include_str!("../../oc-server/src/compat_v1.rs"),
        ),
        ("crates/oc-plugin/tests/js.rs", include_str!("js.rs")),
        (
            "crates/oc-plugin/tests/integration.rs",
            include_str!("integration.rs"),
        ),
    ] {
        let stale = text
            .match_indices("opencode-kiro-auth@")
            .map(|(offset, marker)| {
                text[offset + marker.len()..]
                    .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|found| !found.is_empty() && *found != version)
            .collect::<Vec<_>>();
        assert!(
            stale.is_empty(),
            "{label} still names kiro-auth version(s) {stale:?} while the contract is {version}. \
             A single version in every place is the narrowing; a second one is how the criterion \
             became unsatisfiable in the first place."
        );
    }

    let user_config = Path::new("/config/.config/opencode/opencode.json");
    if let Ok(text) = std::fs::read_to_string(user_config) {
        assert!(
            text.contains(&spec),
            "{} does not pin {spec}; criterion 6 converged on the version the USER'S CONFIG \
             pins, so a config change makes this contract stale rather than merely different",
            user_config.display()
        );
    } else {
        eprintln!(
            "criterion 6: {} is unreadable here, so the user-config half was NOT verified",
            user_config.display()
        );
    }

    let installed = installed_package(Path::new(PLUGIN_CACHE), KIRO_PACKAGE);
    if installed.join("package.json").is_file() {
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(installed.join("package.json")).expect("read kiro manifest"),
        )
        .expect("parse kiro manifest");
        assert_eq!(
            manifest["version"].as_str(),
            Some(version.as_str()),
            "the installed package at {} declares a different version than the spec that \
             resolved it",
            installed.display()
        );
    } else {
        eprintln!(
            "criterion 6: {} is absent here, so the on-disk half was NOT verified",
            installed.display()
        );
    }
}

/// The behavioural replacement for the removed `client.middlewareStack.add` clause.
///
/// Todo 60 established that `middlewareStack` is not a member of
/// `PluginInput.client` — it belongs to the Kiro plugin's own AWS SDK client,
/// constructed inside `makeSdkClient(auth, region, effort)` after the auth loader
/// returns its `fetch` — so the old assertion could never hold at this boundary.
/// What IS observable here is the request metadata the real plugin injects: its
/// `chat.headers` hook sets `x-opencode-kiro-request-kind: compaction` for a
/// compaction turn (`dist/plugin.js:389-397`) and, with its default
/// `diagnostic_log_level: 'off'` (`dist/plugin/config/schema.js:187`), adds no
/// diagnostic identity headers. Both directions are asserted, so a hook that
/// stopped running would fail rather than silently inject nothing.
///
/// The `effort` field is deliberately NOT asserted: it is chosen inside the
/// plugin's AWS client on an outbound Kiro request, which needs live credentials
/// and network this suite forbids. Stating that is the honest scope, not a waiver.
#[tokio::test]
async fn js_real_kiro_plugin_injects_its_request_kind_header_for_a_compaction_turn() {
    // Given
    let cache = PathBuf::from(PLUGIN_CACHE);
    let kiro = installed_package(&cache, KIRO_PACKAGE);
    if !kiro.is_dir() {
        eprintln!(
            "SKIPPED js_real_kiro_plugin_injects_its_request_kind_header_for_a_compaction_turn: \
             {} is absent",
            kiro.display()
        );
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let load = load_js_plugins_ordered(
        vec![JsPluginSpec::new(supported_spec(KIRO_PACKAGE))],
        host(temp.path(), terminal, JsHostPolicy::default()).cache_dir(&cache),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("the real kiro plugin loaded: {:?}", load.diagnostics()));
    assert!(
        plugin
            .manifest()
            .hooks()
            .contains(&oc_plugin::HookName::ChatHeaders),
        "the loaded kiro plugin registers no chat.headers hook, so the assertions below would \
         pass by injecting nothing: {:?}",
        plugin.manifest().hooks()
    );

    let model = kiro_model();
    let provider = ProviderContext {
        source: ProviderSource::Config,
        info: kiro_provider(),
        options: serde_json::Map::new(),
    };

    let foreign_provider = ProviderContext {
        source: ProviderSource::Config,
        info: ResolvedProvider {
            id: "not-kiro".to_owned(),
            ..kiro_provider()
        },
        options: serde_json::Map::new(),
    };

    // When
    let compaction = kiro_headers(plugin.as_ref(), "compaction", &model, &provider).await;
    let ordinary = kiro_headers(plugin.as_ref(), "build", &model, &provider).await;
    let model_only = kiro_headers(plugin.as_ref(), "compaction", &model, &foreign_provider).await;
    load.shutdown().await;

    // Then
    assert_eq!(
        compaction.headers.get(KIRO_REQUEST_KIND_HEADER),
        Some(&"compaction".to_owned()),
        "the real kiro plugin did not inject its request-kind header for a compaction turn; \
         injected: {:?}",
        compaction.headers
    );
    assert!(
        compaction
            .headers
            .keys()
            .all(|name| name.starts_with("x-opencode-kiro-")),
        "every injected header must carry the plugin's own namespace, otherwise something other \
         than the kiro hook wrote them; injected: {:?}",
        compaction.headers
    );
    if let Some(trace) = compaction.headers.get("x-opencode-kiro-diagnostic-trace") {
        let shape = trace
            .chars()
            .map(|c| if c == '-' { '-' } else { 'x' })
            .collect::<String>();
        assert_eq!(
            shape, "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
            "the diagnostic trace header must be the `crypto.randomUUID()` the plugin generates, \
             got {trace:?}"
        );
        assert!(
            trace.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "the diagnostic trace header must be hexadecimal, got {trace:?}"
        );
    } else {
        eprintln!(
            "criterion 6: this host's kiro diagnostic_log_level is 'off', so the diagnostic \
             identity headers were NOT exercised"
        );
    }
    assert!(
        !ordinary.headers.contains_key(KIRO_REQUEST_KIND_HEADER),
        "a non-compaction turn must not receive the request-kind header, otherwise the \
         compaction assertion above would pass for any input; injected: {:?}",
        ordinary.headers
    );
    // Recorded seam, not a friendly double: the hook resolves the provider as
    // `input?.model?.providerID ?? input?.provider?.info?.id` (`dist/plugin.js:390`),
    // and only the second arm ever matches here — `chat_context_value` serializes
    // `ResolvedModel` with Rust field names, so the plugin sees `provider_id`, not
    // the `providerID` upstream's type declares. `chat.message` in the same codec
    // already spells it `providerID` (`jsonrpc.rs:972`), so the two encodings
    // disagree with each other. Kiro survives on its fallback; a plugin reading
    // only `model.providerID` would silently never fire. Pinned here so closing
    // that seam trips this assertion and is a deliberate edit rather than drift.
    assert!(
        model_only.headers.is_empty(),
        "the kiro hook matched on `model.providerID` alone, which means the chat-context encoding \
         now spells the model's provider id the way upstream's type declares. That is a fix, not \
         a failure: update this assertion and remove the seam note above it. Injected: {:?}",
        model_only.headers
    );
}

async fn kiro_headers(
    plugin: &dyn Plugin,
    agent: &str,
    model: &ResolvedModel,
    provider: &ProviderContext,
) -> ChatHeadersOutput {
    let context = ChatContext {
        session_id: "ses_criterion_six",
        agent,
        model,
        provider,
        message: Message::new(Role::User, "summarize"),
    };
    let mut output = ChatHeadersOutput::default();
    plugin
        .call(&mut HookInvocation::ChatHeaders {
            input: &context,
            output: &mut output,
        })
        .await
        .expect("the real kiro chat.headers hook runs");
    output
}

fn kiro_model() -> ResolvedModel {
    ResolvedModel {
        id: "claude-sonnet-4-5".to_owned(),
        provider_id: KIRO_PROVIDER.to_owned(),
        name: "Claude Sonnet 4.5".to_owned(),
        family: "claude".to_owned(),
        release_date: String::new(),
        status: CatalogStatus::Active,
        api: ModelApi::default(),
        capabilities: ModelCapabilities::default(),
        cost: ModelCost::default(),
        limit: ModelLimit::default(),
        options: serde_json::Map::new(),
        headers: BTreeMap::new(),
        variants: BTreeMap::new(),
    }
}

fn kiro_provider() -> ResolvedProvider {
    ResolvedProvider {
        id: KIRO_PROVIDER.to_owned(),
        name: "Kiro".to_owned(),
        env: Vec::new(),
        options: serde_json::Map::new(),
        availability: Availability::none(),
        models: BTreeMap::new(),
    }
}
