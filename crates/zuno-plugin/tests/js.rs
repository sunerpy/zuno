use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use url::Url;
use zuno_engine::terminal_lease::{TerminalBroker, TerminalLease};
use zuno_llm::catalog::availability::{Availability, AvailabilitySource};
use zuno_llm::catalog::models_dev::{CatalogStatus, Interleaved};
use zuno_llm::catalog::resolved::{
    ModelApi, ModelCapabilities, ModelCost, ModelLimit, ResolvedModel, ResolvedProvider,
};
use zuno_llm::event::{Message, Role};
use zuno_paths::ResolvedProject;
use zuno_plugin::{
    AuthApiResult, AuthCredentialResolver, AuthLoader, AuthMethod, AuthPrompt, ChatContext,
    ChatHeadersOutput, ChatParamsOutput, ChatSystemTransformInput, ChatSystemTransformOutput,
    CompactionAutocontinueInput, CompactionAutocontinueOutput, HookInvocation, JsDiagnosticKind,
    JsHostBuilder, JsHostConfig, JsHostPolicy, JsPluginInput, JsPluginLoad, JsPluginSpec, Plugin,
    ProviderContext, ProviderHookContext, ProviderSmallModelInput, ProviderSmallModelOutput,
    ProviderSource, SUPPORTED_JS_PLUGINS, TextCompleteInput, TextCompleteOutput, discover_runtime,
    load_js_plugins_ordered,
};
use zuno_testkit::FakeTerminalOwner;

/// The provider id the kiro plugin registers, from `dist/plugin.js`'s `KIRO_PROVIDER_ID`.
const KIRO_PROVIDER: &str = "kiro-auth";

/// The header kiro injects for a compaction turn, from `dist/core/request/request-kind.js:2`.
const KIRO_REQUEST_KIND_HEADER: &str = "x-opencode-kiro-request-kind";

const FIXTURE: &str = r#"
import { existsSync, writeFileSync } from "node:fs";

const providerKeys = ["id", "name", "source", "env", "key", "options", "models"];
const legacyModelKeys = [
  "id", "providerID", "api", "name", "capabilities", "cost", "limit", "status", "options", "headers",
];
const v2ModelKeys = [...legacyModelKeys, "family", "release_date", "variants"];

function assertAllowedKeys(value, allowed, label) {
  const extras = Object.keys(value).filter((key) => !allowed.includes(key));
  if (extras.length > 0) throw new Error(`${label} leaked non-SDK keys: ${extras.join(",")}`);
}

function assertRequiredKeys(value, required, label) {
  const missing = required.filter((key) => !(key in value));
  if (missing.length > 0) throw new Error(`${label} omitted SDK keys: ${missing.join(",")}`);
}

function assertModel(model, generation, label) {
  const allowed = generation === "legacy" ? legacyModelKeys : v2ModelKeys;
  assertAllowedKeys(model, allowed, label);
  assertRequiredKeys(model, legacyModelKeys, label);
  if (model.providerID !== "resident-fixture") {
    throw new Error(`${label}.providerID was ${JSON.stringify(model.providerID)}`);
  }
  if ("provider_id" in model) throw new Error(`${label} exposed provider_id`);
  assertAllowedKeys(model.api, ["id", "url", "npm"], `${label}.api`);
  if ("endpoint" in model.api) throw new Error(`${label}.api exposed endpoint`);
  if (generation === "legacy") {
    if ("family" in model || "release_date" in model || "variants" in model) {
      throw new Error(`${label} exposed v2-only model fields`);
    }
    if ("interleaved" in model.capabilities) {
      throw new Error(`${label}.capabilities exposed interleaved`);
    }
    if ("input" in model.limit) throw new Error(`${label}.limit exposed input`);
  } else {
    assertRequiredKeys(model, ["release_date"], label);
    if (!("interleaved" in model.capabilities)) {
      throw new Error(`${label}.capabilities omitted interleaved`);
    }
  }
}

function assertProvider(provider, generation, source, key, label) {
  assertAllowedKeys(provider, providerKeys, label);
  assertRequiredKeys(provider, ["id", "name", "source", "env", "options", "models"], label);
  if (provider.source !== source) {
    throw new Error(`${label}.source was ${JSON.stringify(provider.source)}, expected ${source}`);
  }
  if (key === undefined ? "key" in provider : provider.key !== key) {
    throw new Error(`${label}.key was ${JSON.stringify(provider.key)}, expected ${JSON.stringify(key)}`);
  }
  if ("availability" in provider) throw new Error(`${label} exposed availability`);
  for (const [id, model] of Object.entries(provider.models)) {
    assertModel(model, generation, `${label}.models.${id}`);
  }
}

function modelBase(id, providerID) {
  return {
    id,
    providerID,
    api: { id: `${id}-wire`, url: "https://example.invalid/v1", npm: "@ai-sdk/openai-compatible" },
    name: `SDK ${id}`,
    capabilities: {
      temperature: true,
      reasoning: false,
      attachment: false,
      toolcall: true,
      input: { text: true, audio: false, image: false, video: false, pdf: false },
      output: { text: true, audio: false, image: false, video: false, pdf: false },
    },
    cost: { input: 0, output: 0, cache: { read: 0, write: 0 } },
    limit: { context: 8192, output: 2048 },
    status: "active",
    options: {},
    headers: {},
  };
}

function legacyModel(id) {
  return modelBase(id, "resident-fixture");
}

function v2Model(id) {
  const model = modelBase(id, "resident-fixture");
  model.family = "sdk-v2";
  model.capabilities.interleaved = false;
  model.limit.input = 4096;
  model.release_date = "2026-08-12";
  model.variants = {};
  return model;
}

function assertLegacyContext(input, label) {
  assertModel(input.model, "legacy", `${label}.model`);
  if (input.provider.source !== "config") {
    throw new Error(`${label}.provider.source was ${JSON.stringify(input.provider.source)}`);
  }
  assertProvider(input.provider.info, "legacy", "config", undefined, `${label}.provider.info`);
}

export default {
  id: "resident-fixture",
  server: async (_input, options) => ({
    auth: {
      provider: "resident-fixture",
      loader: async (getAuth, provider) => {
        if (options?.assertSdkAuthProvider) {
          const auth = await getAuth();
          assertProvider(provider, "legacy", "api", auth.key, "Auth.loader.provider");
          provider.models["auth-sdk-model"] = legacyModel("auth-sdk-model");
          return { sdkBoundary: `${provider.source}:${provider.key}` };
        }
        if (options?.forgeLegacyOnlyFields) {
          for (const [id, model] of Object.entries(provider.models)) {
            // Throws if any legacy-omitted field is visible here, so this call
            // is what proves the forward projection hides all five.
            assertModel(model, "legacy", `Auth.loader.provider.models.${id}`);
            model.family = "forged-family";
            model.release_date = "1999-12-31";
            model.variants = { forged: { thinkingBudget: 1 } };
            model.capabilities.interleaved = "forged-interleaved";
            model.limit.input = 111;
          }
          return {};
        }
        if (options?.mutateProviderDeep) {
          const deep = {};
          let current = deep;
          for (let depth = 0; depth < 18; depth += 1) {
            current.next = {};
            current = current.next;
          }
          provider.options.pluginDeep = deep;
          return {};
        }
        if (!options?.returnDeep) return {};
        const result = {};
        let current = result;
        for (let depth = 0; depth < 18; depth += 1) {
          current.next = {};
          current = current.next;
        }
        return result;
      },
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
    provider: {
      id: "resident-fixture",
      models: async (provider, context) => {
        if (!options?.assertSdkProviderHook) return {};
        assertProvider(provider, "v2", "api", context.auth.key, "ProviderHook.models.provider");
        return { "provider-sdk-model": v2Model("provider-sdk-model") };
      },
    },
    "chat.params": async (input, output) => {
      if (!options?.assertSdkOrdinaryHooks) return;
      assertLegacyContext(input, "chat.params");
      output.options.sdkParams = true;
    },
    "chat.headers": async (input, output) => {
      if (!options?.assertSdkOrdinaryHooks) return;
      assertLegacyContext(input, "chat.headers");
      output.headers["x-sdk-headers"] = "projected";
    },
    "experimental.chat.system.transform": async (input, output) => {
      if (!options?.assertSdkOrdinaryHooks) return;
      assertModel(input.model, "legacy", "experimental.chat.system.transform.model");
      output.system.push("sdk-system");
    },
    "experimental.provider.small_model": async (input, output) => {
      if (!options?.assertSdkOrdinaryHooks) return;
      assertProvider(input.provider, "v2", "config", undefined, "experimental.provider.small_model.provider");
      assertModel(output.model, "v2", "experimental.provider.small_model.output.model");
      output.model = v2Model("small-sdk-model");
    },
    "experimental.compaction.autocontinue": async (input, output) => {
      if (!options?.assertSdkOrdinaryHooks) return;
      assertLegacyContext(input, "experimental.compaction.autocontinue");
      output.enabled = false;
    },
    "experimental.text.complete": async (_input, output) => {
      if (options?.hangOnce && !existsSync(options.hangOnce)) {
        writeFileSync(options.hangOnce, "hung");
        await new Promise(() => {});
      }
      output.text += "-resident";
    },
    "tool.execute.before": async (_input, output) => {
      if (options?.mutateExistingDeep) {
        let current = output.args.deep;
        while (current.next) current = current.next;
        current.sentinel = "mutated";
      }
      if (options?.mutateBeforeTruncation) {
        output.args.shallow = "mutated";
        const deep = {};
        let current = deep;
        for (let depth = 0; depth < 18; depth += 1) {
          current.next = {};
          current = current.next;
        }
        output.args.deep = deep;
      }
    },
    "tool.execute.after": async (_input, output) => {
      if (options?.mutateAcrossArguments) output.title = "mutated";
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

fn npm_compatibility_fixture(root: &Path, range: &str, marker: &Path) -> JsPluginSpec {
    let package =
        root.join("cache/packages/compatibility-fixture@1.0.0/node_modules/compatibility-fixture");
    std::fs::create_dir_all(&package).expect("create compatibility fixture package");
    std::fs::write(
        package.join("package.json"),
        serde_json::to_vec(&serde_json::json!({
            "name": "compatibility-fixture",
            "version": "1.0.0",
            "type": "module",
            "main": "index.js",
            "engines": { "opencode": range },
        }))
        .expect("encode compatibility fixture manifest"),
    )
    .expect("write compatibility fixture manifest");
    let marker =
        serde_json::to_string(&marker.to_string_lossy()).expect("encode compatibility marker path");
    std::fs::write(
        package.join("index.js"),
        format!(
            r#"import {{ writeFileSync }} from "node:fs";
export default {{
  id: "compatibility-fixture",
  server: async () => {{
    writeFileSync({marker}, "activated");
    return {{ "experimental.text.complete": async (_input, output) => {{ output.text += "-compat"; }} }};
  }},
}};
"#,
        ),
    )
    .expect("write compatibility fixture entrypoint");
    let sdk = package.join("node_modules/@opencode-ai/sdk");
    std::fs::create_dir_all(sdk.join("dist")).expect("create compatibility fixture SDK");
    std::fs::write(
        sdk.join("package.json"),
        r#"{"name":"@opencode-ai/sdk","type":"module","main":"dist/index.js"}"#,
    )
    .expect("write compatibility fixture SDK manifest");
    std::fs::write(
        sdk.join("dist/index.js"),
        "export function createOpencodeClient() { return {}; }",
    )
    .expect("write compatibility fixture SDK entrypoint");
    JsPluginSpec::new("compatibility-fixture@1.0.0")
}

async fn record_sdk_client_config(case: &str) -> serde_json::Value {
    let temp = tempfile::tempdir().expect("tempdir");
    let entry = temp.path().join("plugin.mjs");
    std::fs::write(
        &entry,
        r#"export default { id: "sdk-auth-fixture", server: async () => ({}) };"#,
    )
    .expect("write plugin fixture");

    let recorded = temp.path().join(format!("sdk-client-{case}.json"));
    let sdk = temp.path().join("sdk");
    std::fs::create_dir_all(sdk.join("dist")).expect("create fake SDK");
    std::fs::write(sdk.join("package.json"), r#"{"type":"module"}"#)
        .expect("write fake SDK manifest");
    let recorded_literal =
        serde_json::to_string(&recorded.to_string_lossy()).expect("encode recording path");
    std::fs::write(
        sdk.join("dist/index.js"),
        format!(
            r#"import {{ writeFileSync }} from "node:fs";
export function createOpencodeClient(config) {{
  writeFileSync({recorded_literal}, JSON.stringify(config));
  return {{}};
}}"#,
        ),
    )
    .expect("write fake SDK");

    let runtime = discover_runtime(&["sdk-auth-fixture".to_owned()]).expect("JavaScript runtime");
    let input =
        JsPluginInput::new(temp.path(), temp.path(), "http://127.0.0.1:4096").with_sdk_module(&sdk);
    let host = JsHostBuilder::new(
        "sdk-auth-fixture",
        runtime,
        &file_spec(&entry),
        &entry,
        input,
    )
    .start()
    .await
    .expect("start SDK auth fixture");
    host.shutdown().await;

    serde_json::from_slice(&std::fs::read(recorded).expect("read SDK client config"))
        .expect("decode SDK client config")
}

#[tokio::test]
async fn js_sdk_client_authenticates_to_the_password_gated_server() {
    const CHILD_CASE: &str = "OC_JS_SDK_AUTH_TEST_CASE";
    if let Ok(case) = std::env::var(CHILD_CASE) {
        let config = record_sdk_client_config(&case).await;
        let authorization = config
            .pointer("/headers/Authorization")
            .and_then(serde_json::Value::as_str);
        match case.as_str() {
            "default-user" => {
                assert_eq!(authorization, Some("Basic b3BlbmNvZGU6c2VjcmV0"));
            }
            "custom-user" => {
                assert_eq!(authorization, Some("Basic YWxpY2U6c2VjcmV0"));
            }
            "empty-password" => assert_eq!(authorization, None),
            _ => panic!("unknown child case {case}"),
        }
        return;
    }

    for (case, username, password) in [
        ("default-user", None, "secret"),
        ("custom-user", Some("alice"), "secret"),
        ("empty-password", Some("alice"), ""),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
        command
            .arg("--exact")
            .arg("js_sdk_client_authenticates_to_the_password_gated_server")
            .arg("--nocapture")
            .env(CHILD_CASE, case)
            .env("OPENCODE_SERVER_PASSWORD", password);
        if let Some(username) = username {
            command.env("OPENCODE_SERVER_USERNAME", username);
        } else {
            command.env_remove("OPENCODE_SERVER_USERNAME");
        }
        let status = command.status().expect("run isolated SDK auth test");
        assert!(status.success(), "SDK auth child case {case} failed");
    }
}

async fn fixture_auth_loader(
    root: &Path,
    options: Option<serde_json::Value>,
) -> (JsPluginLoad, Arc<dyn AuthLoader>) {
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let mut spec = file_spec(&fixture(root));
    if let Some(options) = options {
        spec = spec.options(options);
    }
    let load =
        load_js_plugins_ordered(vec![spec], host(root, terminal, JsHostPolicy::default())).await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let loader = plugin
        .auth()
        .and_then(|auth| auth.loader)
        .expect("fixture auth loader");
    (load, loader)
}

struct MissingCredential;

#[async_trait::async_trait]
impl AuthCredentialResolver for MissingCredential {
    async fn resolve(&self) -> Result<Option<zuno_auth::Credential>, zuno_error::BoxSource> {
        Ok(None)
    }
}

struct ApiCredential;

#[async_trait::async_trait]
impl AuthCredentialResolver for ApiCredential {
    async fn resolve(&self) -> Result<Option<zuno_auth::Credential>, zuno_error::BoxSource> {
        Ok(Some(zuno_auth::Credential::Api {
            key: zuno_auth::Secret::new("sdk-secret"),
            metadata: None,
        }))
    }
}

fn sdk_boundary_provider(source: AvailabilitySource) -> ResolvedProvider {
    let mut model = kiro_model();
    model.id = "sdk-input-model".to_owned();
    model.provider_id = "resident-fixture".to_owned();
    let mut provider = kiro_provider();
    provider.id = "resident-fixture".to_owned();
    provider.models.insert(model.id.clone(), model);
    provider.availability.record(source);
    provider
}

/// A canonical provider whose model carries a non-default value for every field
/// the legacy SDK surface omits.
///
/// Each value must differ from what `plugin_model_value` refills a missing key
/// with, or the restoration it guards becomes an equivalent mutant: the four
/// fields other than `family` are defaulted in `kiro_model`, which is why
/// deleting their restorations broke no test before this fixture existed.
fn legacy_only_field_provider() -> ResolvedProvider {
    let mut model = kiro_model();
    model.id = "legacy-only-field-model".to_owned();
    model.provider_id = "resident-fixture".to_owned();
    model.family = "canonical-family".to_owned();
    model.release_date = "2026-02-01".to_owned();
    model.variants.insert(
        "thinking".to_owned(),
        serde_json::json!({ "thinkingBudget": 8192 })
            .as_object()
            .expect("a variant body is an object")
            .clone(),
    );
    model.capabilities.interleaved = Interleaved::Name("reasoning_content".to_owned());
    model.limit.input = Some(200_000.0);
    let mut provider = kiro_provider();
    provider.id = "resident-fixture".to_owned();
    provider.models.insert(model.id.clone(), model);
    provider
}

fn provider_with_variant_depth(nested_objects: usize) -> ResolvedProvider {
    let mut depth_probe = serde_json::json!({ "thinkingBudget": 8192 });
    for _ in 0..nested_objects {
        depth_probe = serde_json::json!({ "next": depth_probe });
    }
    let mut model = kiro_model();
    model.id = "antigravity-claude-opus-4-5-thinking".to_owned();
    model.provider_id = "resident-fixture".to_owned();
    model.options.insert("depthProbe".to_owned(), depth_probe);
    let mut provider = kiro_provider();
    provider.id = "resident-fixture".to_owned();
    provider.models.insert(model.id.clone(), model);
    provider
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
async fn js_auth_loader_round_trips_provider_data_below_its_depth_bound_byte_identically() {
    // Given: provider data reaching one object below the measured production cap.
    let temp = tempfile::tempdir().expect("tempdir");
    let (load, loader) = fixture_auth_loader(temp.path(), None).await;
    let mut provider = provider_with_variant_depth(11);
    let before = serde_json::to_vec(&provider).expect("serialize provider before loader");

    // When
    let options = loader
        .load(&MissingCredential, &mut provider)
        .await
        .expect("provider below the per-argument depth bound round-trips");
    load.shutdown().await;

    // Then
    assert!(options.is_empty());
    assert_eq!(
        serde_json::to_vec(&provider).expect("serialize provider after loader"),
        before,
        "a no-op JavaScript auth loader must return every provider byte unchanged"
    );
}

#[tokio::test]
async fn js_auth_loader_restores_host_truncated_provider_byte_identically() {
    // Given: host-supplied provider data reaching the defensive cap.
    let temp = tempfile::tempdir().expect("tempdir");
    let (load, loader) = fixture_auth_loader(temp.path(), None).await;
    let mut provider = provider_with_variant_depth(12);
    let before = serde_json::to_vec(&provider).expect("serialize provider before loader");

    // When
    let options = loader
        .load(&MissingCredential, &mut provider)
        .await
        .expect("host-owned provider branches are restored after bounded encoding");
    load.shutdown().await;

    // Then
    assert!(options.is_empty());
    assert_eq!(
        serde_json::to_vec(&provider).expect("serialize provider after restoration"),
        before,
        "a no-op loader must restore every host-owned byte beyond the transport cap"
    );
}

#[tokio::test]
async fn js_auth_loader_refuses_plugin_truncated_provider_and_preserves_the_original() {
    // Given: the plugin itself adds provider data beyond the defensive cap.
    let temp = tempfile::tempdir().expect("tempdir");
    let (load, loader) = fixture_auth_loader(
        temp.path(),
        Some(serde_json::json!({ "mutateProviderDeep": true })),
    )
    .await;
    let mut provider = kiro_provider();
    let before = serde_json::to_vec(&provider).expect("serialize provider before loader");

    // When
    let error = loader
        .load(&MissingCredential, &mut provider)
        .await
        .expect_err("plugin-origin provider loss must be refused");
    load.shutdown().await;

    // Then
    let message = error.to_string();
    assert!(
        message.contains("resident-fixture.mjs"),
        "plugin-origin loss must name the responsible plugin: {message}"
    );
    assert!(
        message.contains(&format!("/options/pluginDeep{}", "/next".repeat(14))),
        "the refusal must name the plugin-origin provider path: {message}"
    );
    assert_eq!(
        serde_json::to_vec(&provider).expect("serialize provider after refusal"),
        before,
        "refusing plugin-origin loss must leave the real provider untouched"
    );
}

#[tokio::test]
async fn js_auth_loader_still_bounds_an_arbitrary_plugin_return_graph() {
    // Given
    let temp = tempfile::tempdir().expect("tempdir");
    let (load, loader) =
        fixture_auth_loader(temp.path(), Some(serde_json::json!({ "returnDeep": true }))).await;
    let mut provider = kiro_provider();

    // When
    let options = loader
        .load(&MissingCredential, &mut provider)
        .await
        .expect("the bounded return value remains a valid JSON object");
    load.shutdown().await;

    // Then: the sixteenth nested object is a marker, so the walk remains bounded.
    let mut value = options.get("next").expect("first nested value");
    for _ in 1..16 {
        value = value.get("next").expect("nested value before the cap");
    }
    assert_eq!(
        value.get("$truncated"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        value.get("$path").and_then(serde_json::Value::as_str),
        Some("/next/next/next/next/next/next/next/next/next/next/next/next/next/next/next/next")
    );
}

#[tokio::test]
async fn js_auth_loader_restores_every_legacy_only_model_field_the_sdk_surface_omits() {
    // Given: a canonical model carrying a non-default value for each field the
    // legacy `model_value` projection omits, and a no-op JavaScript auth loader.
    let temp = tempfile::tempdir().expect("tempdir");
    let (load, loader) = fixture_auth_loader(temp.path(), None).await;
    let mut provider = legacy_only_field_provider();
    let canonical = provider
        .models
        .get("legacy-only-field-model")
        .expect("the canonical model exists before the loader runs")
        .clone();

    // When: the provider round-trips through the real JavaScript process.
    loader
        .load(&MissingCredential, &mut provider)
        .await
        .expect("a no-op auth loader round-trips the provider");
    load.shutdown().await;

    // Then: every legacy-omitted field survives. The legacy surface never sent
    // these to JavaScript, so the returned object cannot carry them and
    // `plugin_model_value` refills each with a default; only the reverse
    // projection's restoration can recover the canonical value.
    let model = provider
        .models
        .get("legacy-only-field-model")
        .expect("the canonical model survives the loader");
    assert_eq!(
        model.family, canonical.family,
        "the legacy reverse projection must restore `family` the SDK surface omits"
    );
    assert_eq!(
        model.release_date, canonical.release_date,
        "the legacy reverse projection must restore `release_date` the SDK surface omits"
    );
    assert_eq!(
        model.variants, canonical.variants,
        "the legacy reverse projection must restore `variants` the SDK surface omits"
    );
    assert_eq!(
        model.capabilities.interleaved, canonical.capabilities.interleaved,
        "the legacy reverse projection must restore `capabilities.interleaved` the SDK surface omits"
    );
    assert_eq!(
        model.limit.input, canonical.limit.input,
        "the legacy reverse projection must restore `limit.input` the SDK surface omits"
    );
}

#[tokio::test]
async fn js_auth_loader_refuses_legacy_only_model_fields_a_plugin_forges() {
    // Given: a JavaScript auth loader that asserts all five legacy-omitted
    // fields are hidden from it, then writes a value into each anyway.
    let temp = tempfile::tempdir().expect("tempdir");
    let (load, loader) = fixture_auth_loader(
        temp.path(),
        Some(serde_json::json!({ "forgeLegacyOnlyFields": true })),
    )
    .await;
    let mut provider = legacy_only_field_provider();
    let canonical = provider
        .models
        .get("legacy-only-field-model")
        .expect("the canonical model exists before the loader runs")
        .clone();

    // When
    loader
        .load(&MissingCredential, &mut provider)
        .await
        .expect("a forging auth loader still round-trips the provider");
    load.shutdown().await;

    // Then: a legacy-generation plugin cannot introduce a field it was never
    // shown. `plugin_model_value` leaves an occupied key alone, so without the
    // restoration each forged value would reach the resolved catalog.
    let model = provider
        .models
        .get("legacy-only-field-model")
        .expect("the canonical model survives the loader");
    assert_eq!(
        model.family, canonical.family,
        "a legacy plugin must not forge `family`"
    );
    assert_eq!(
        model.release_date, canonical.release_date,
        "a legacy plugin must not forge `release_date`"
    );
    assert_eq!(
        model.variants, canonical.variants,
        "a legacy plugin must not forge `variants`"
    );
    assert_eq!(
        model.capabilities.interleaved, canonical.capabilities.interleaved,
        "a legacy plugin must not forge `capabilities.interleaved`"
    );
    assert_eq!(
        model.limit.input, canonical.limit.input,
        "a legacy plugin must not forge `limit.input`"
    );
}

#[tokio::test]
async fn js_sdk_boundary_auth_loader_reads_provider_and_supplies_model() {
    // Given: a real JavaScript Auth.loader that reads the declared legacy Provider
    // shape and inserts a legacy SDK Model constructed from scratch.
    let temp = tempfile::tempdir().expect("tempdir");
    let (load, loader) = fixture_auth_loader(
        temp.path(),
        Some(serde_json::json!({ "assertSdkAuthProvider": true })),
    )
    .await;
    let mut provider = sdk_boundary_provider(AvailabilitySource::StoredApiKey);

    // When
    let options = loader
        .load(&ApiCredential, &mut provider)
        .await
        .expect("SDK-shaped Auth.loader provider round-trips");
    load.shutdown().await;

    // Then
    assert_eq!(
        options
            .get("sdkBoundary")
            .and_then(serde_json::Value::as_str),
        Some("api:sdk-secret")
    );
    let model = provider
        .models
        .get("auth-sdk-model")
        .expect("legacy SDK model inserted by the JavaScript loader");
    assert_eq!(model.provider_id, "resident-fixture");
    assert_eq!(model.family, "");
    assert_eq!(model.release_date, "");
    assert!(model.variants.is_empty());
}

#[tokio::test]
async fn js_sdk_boundary_provider_loader_reads_v2_provider_and_supplies_model() {
    // Given: a real JavaScript ProviderHook.models callback that validates its
    // ProviderV2 argument and returns a ModelV2 constructed from scratch.
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let spec = file_spec(&fixture(temp.path()))
        .options(serde_json::json!({ "assertSdkProviderHook": true }));
    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let loader = plugin
        .provider()
        .and_then(|provider| provider.models)
        .expect("fixture provider model loader");
    let provider = sdk_boundary_provider(AvailabilitySource::StoredApiKey);
    let credential = zuno_auth::Credential::Api {
        key: zuno_auth::Secret::new("sdk-secret"),
        metadata: None,
    };

    // When
    let models = loader
        .models(
            &provider,
            ProviderHookContext {
                auth: Some(&credential),
            },
        )
        .await
        .expect("SDK-shaped ProviderHook.models argument and return");
    load.shutdown().await;

    // Then
    let model = models
        .get("provider-sdk-model")
        .expect("ModelV2 returned by the real JavaScript provider hook");
    assert_eq!(model.provider_id, "resident-fixture");
    assert_eq!(model.family, "sdk-v2");
    assert_eq!(model.release_date, "2026-08-12");
}

#[tokio::test]
async fn js_sdk_boundary_ordinary_hooks_read_declared_shapes_and_supply_small_model() {
    // Given: one real JavaScript plugin implementing every ordinary hook whose
    // contract carries a legacy Model/Provider or v2 Model/Provider.
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let spec = file_spec(&fixture(temp.path()))
        .options(serde_json::json!({ "assertSdkOrdinaryHooks": true }));
    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let model = {
        let mut model = kiro_model();
        model.provider_id = "resident-fixture".to_owned();
        model
    };
    let provider = ProviderContext {
        source: ProviderSource::Config,
        info: sdk_boundary_provider(AvailabilitySource::ConfigBlock),
        options: serde_json::Map::new(),
    };
    let context = ChatContext {
        session_id: "ses_sdk_boundary",
        agent: "build",
        model: &model,
        provider: &provider,
        message: Message::new(Role::User, "project every model boundary"),
    };

    // When: exercise all three chat-context users plus the standalone legacy
    // model hook and the bidirectional ProviderV2/ModelV2 hook.
    let mut params = ChatParamsOutput::default();
    plugin
        .call(&mut HookInvocation::ChatParams {
            input: &context,
            output: &mut params,
        })
        .await
        .expect("chat.params sees SDK shape");
    let mut headers = ChatHeadersOutput::default();
    plugin
        .call(&mut HookInvocation::ChatHeaders {
            input: &context,
            output: &mut headers,
        })
        .await
        .expect("chat.headers sees SDK shape");
    let mut autocontinue = CompactionAutocontinueOutput { enabled: true };
    plugin
        .call(&mut HookInvocation::CompactionAutocontinue {
            input: &CompactionAutocontinueInput {
                context: &context,
                overflow: true,
            },
            output: &mut autocontinue,
        })
        .await
        .expect("compaction autocontinue sees SDK shape");
    let mut system = ChatSystemTransformOutput::default();
    plugin
        .call(&mut HookInvocation::ChatSystemTransform {
            input: &ChatSystemTransformInput {
                session_id: Some("ses_sdk_boundary"),
                model: &model,
            },
            output: &mut system,
        })
        .await
        .expect("system transform sees legacy SDK model");
    let mut small = ProviderSmallModelOutput {
        model: Some(model.clone()),
    };
    plugin
        .call(&mut HookInvocation::ProviderSmallModel {
            input: &ProviderSmallModelInput {
                provider: &provider.info,
            },
            output: &mut small,
        })
        .await
        .expect("small-model hook projects both directions");
    load.shutdown().await;

    // Then: every callback made an observable mutation after its shape checks.
    assert_eq!(
        params.options.get("sdkParams"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        headers.headers.get("x-sdk-headers").map(String::as_str),
        Some("projected")
    );
    assert!(!autocontinue.enabled);
    assert_eq!(system.system, ["sdk-system"]);
    let small = small.model.expect("JavaScript supplied a ModelV2");
    assert_eq!(small.id, "small-sdk-model");
    assert_eq!(small.provider_id, "resident-fixture");
    assert_eq!(small.family, "sdk-v2");
}

#[tokio::test]
async fn js_ordinary_hook_refuses_truncation_before_committing_any_output_field() {
    // Given: the plugin itself adds both a shallow mutation and an object graph
    // that reaches the encoder's cap.
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let spec = file_spec(&fixture(temp.path())).options(serde_json::json!({
        "mutateBeforeTruncation": true,
    }));
    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let mut output = zuno_plugin::ToolExecuteBeforeOutput {
        args: serde_json::json!({
            "shallow": "original",
        }),
    };
    let before = output.clone();

    // When
    let error = plugin
        .call(&mut HookInvocation::ToolExecuteBefore {
            input: &zuno_plugin::ToolExecuteBeforeInput {
                tool: "fixture",
                session_id: "session",
                call_id: "call",
            },
            output: &mut output,
        })
        .await
        .expect_err("a lossy ordinary-hook output must be refused");
    load.shutdown().await;

    // Then
    let message = error.to_string();
    assert!(
        message.contains("resident-fixture.mjs"),
        "the refusal must name the plugin: {message}"
    );
    assert!(
        message.contains("`tool.execute.before` hook argument 1"),
        "the refusal must name the mutable hook argument: {message}"
    );
    assert!(
        message.contains(&format!("/args/deep{}", "/next".repeat(14))),
        "the refusal must name the argument-relative JSON Pointer: {message}"
    );
    assert_eq!(
        output, before,
        "detecting any truncated field must preserve every original output field"
    );
}

#[tokio::test]
async fn js_noop_hook_restores_host_truncated_input_without_blaming_the_plugin() {
    // Given: host-owned argument 0 reaches the depth cap and the callback is a no-op.
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let spec = file_spec(&fixture(temp.path()));
    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let mut deep = serde_json::json!({ "sentinel": "preserved" });
    for _ in 0..16 {
        deep = serde_json::json!({ "next": deep });
    }
    let args = serde_json::json!({ "deep": deep });
    let mut output = zuno_tool::ToolOutput::text("original", "output");
    let before = output.clone();

    // When
    plugin
        .call(&mut HookInvocation::ToolExecuteAfter {
            input: &zuno_plugin::ToolExecuteAfterInput {
                tool: "fixture",
                session_id: "session",
                call_id: "call",
                args: &args,
            },
            output: &mut output,
        })
        .await
        .expect("host-side encoder loss must not fail or disable a no-op plugin");

    // Then
    assert_eq!(
        output, before,
        "the no-op hook must preserve the mutable output byte-for-byte"
    );
    assert!(
        !plugin.is_disabled(),
        "host loss must not disable the plugin"
    );
    assert!(
        plugin.diagnostics().is_empty(),
        "host loss must not publish a plugin-fault diagnostic: {:?}",
        plugin.diagnostics()
    );
    load.shutdown().await;
}

#[tokio::test]
async fn js_plugin_mutation_below_a_host_cutoff_is_still_refused() {
    // Given: the plugin mutates inside an existing host object below the cutoff.
    let temp = tempfile::tempdir().expect("tempdir");
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));
    let spec = file_spec(&fixture(temp.path())).options(serde_json::json!({
        "mutateExistingDeep": true,
    }));
    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;
    let plugin = load
        .plugins()
        .first()
        .unwrap_or_else(|| panic!("fixture plugin loaded: {:?}", load.diagnostics()));
    let mut deep = serde_json::json!({ "sentinel": "preserved" });
    for _ in 0..16 {
        deep = serde_json::json!({ "next": deep });
    }
    let mut output = zuno_plugin::ToolExecuteBeforeOutput {
        args: serde_json::json!({ "deep": deep }),
    };
    let before = output.clone();

    // When
    let error = plugin
        .call(&mut HookInvocation::ToolExecuteBefore {
            input: &zuno_plugin::ToolExecuteBeforeInput {
                tool: "fixture",
                session_id: "session",
                call_id: "call",
            },
            output: &mut output,
        })
        .await
        .expect_err("a plugin mutation below a host cutoff must not be restored away");
    load.shutdown().await;

    // Then
    let message = error.to_string();
    assert!(
        message.contains("resident-fixture.mjs")
            && message.contains(&format!("/args/deep{}", "/next".repeat(14))),
        "the refusal must name the plugin and affected cutoff: {message}"
    );
    assert_eq!(
        output, before,
        "a refused deep plugin mutation must not commit any output"
    );
}

#[tokio::test]
async fn js_timed_out_hook_is_permanently_disabled_before_the_next_invocation() {
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
        .expect_err("the direct call reports the failure after disabling the plugin");
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
        .expect("a disabled plugin makes later invocations a no-op");

    // Then
    assert_eq!(first.text, "first");
    assert_eq!(second.text, "second");
    assert!(plugin.is_disabled());
    assert_eq!(plugin.restart_count(), 0);
    assert!(
        plugin
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.kind == JsDiagnosticKind::TimedOut
                && diagnostic.hook.as_deref() == Some("experimental.text.complete"))
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

#[tokio::test]
async fn js_version_gate_skips_an_excluding_engines_opencode_package_before_activation() {
    // Given: an installed npm plugin whose package contract excludes this host.
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("activated");
    let spec = npm_compatibility_fixture(temp.path(), ">=2.0.0", &marker);
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));

    // When: the production loader resolves the npm package.
    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;

    // Then: compatibility fails before import/factory activation and names both versions.
    assert!(load.plugins().is_empty(), "{:?}", load.diagnostics());
    assert!(!marker.exists(), "the incompatible plugin factory ran");
    assert_eq!(load.diagnostics().len(), 1);
    let diagnostic = &load.diagnostics()[0];
    assert_eq!(diagnostic.kind, JsDiagnosticKind::Compatibility);
    assert_eq!(diagnostic.plugin, "compatibility-fixture@1.0.0");
    assert!(diagnostic.message.contains("requires opencode >=2.0.0"));
    assert!(diagnostic.message.contains("running 1.18.13"));
}

#[tokio::test]
async fn js_version_gate_loads_a_satisfying_engines_opencode_package() {
    // Given: an installed npm plugin whose package contract admits this host.
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("activated");
    let spec = npm_compatibility_fixture(temp.path(), ">=1.18.0 <2.0.0", &marker);
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));

    // When
    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;

    // Then
    assert_eq!(load.plugins().len(), 1, "{:?}", load.diagnostics());
    assert!(load.diagnostics().is_empty());
    assert!(marker.exists(), "the compatible plugin factory did not run");
    load.shutdown().await;
}

#[tokio::test]
async fn js_version_gate_rejects_a_non_semver_engines_opencode_range() {
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("activated");
    let spec = npm_compatibility_fixture(temp.path(), "latest", &marker);
    let owner = Arc::new(FakeTerminalOwner::new());
    let terminal: Arc<dyn TerminalLease> = Arc::new(TerminalBroker::new(owner));

    let load = load_js_plugins_ordered(
        vec![spec],
        host(temp.path(), terminal, JsHostPolicy::default()),
    )
    .await;

    assert!(load.plugins().is_empty(), "{:?}", load.diagnostics());
    assert!(!marker.exists(), "the invalid-range plugin factory ran");
    assert_eq!(load.diagnostics().len(), 1);
    assert_eq!(load.diagnostics()[0].kind, JsDiagnosticKind::Compatibility);
    assert!(load.diagnostics()[0].message.contains("opencode latest"));
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
        .unwrap_or_else(|| panic!("zuno_plugin::SUPPORTED_JS_PLUGINS no longer lists {package}"));
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
    let root = zuno_testkit::subject::workspace_root().expect("workspace root");

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
            "crates/zuno-server/src/compat_v1.rs",
            include_str!("../../zuno-server/src/compat_v1.rs"),
        ),
        ("crates/zuno-plugin/tests/js.rs", include_str!("js.rs")),
        (
            "crates/zuno-plugin/tests/integration.rs",
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

    // The provenance half, and it runs on every host.
    //
    // This used to read `/config/.config/opencode/opencode.json` under an
    // `if let Ok(...)`, because the criterion's anchor was worded as "the version
    // the USER'S CONFIG pins". Two things were wrong with that. The anchor pointed
    // outside the repository, so no reviewer could check it; and the `if let Ok`
    // made the assertion evaporate on any machine without that developer's file,
    // which is the unfalsifiable-gate shape three other defects in this project
    // had. The pin now lives in `SUPPORTED_JS_PLUGINS`, and what the capture is
    // held to is that it still records *where* that pin was observed — so a
    // refreshed capture cannot leave a bare version string with no source.
    let provenance = recorded_plugin_provenance(&capture, &spec).unwrap_or_else(|| {
        panic!(
            "docs/v1-surface-capture.md has no `Installed plugins` row for `{spec}`. The capture \
             is the committed record of where this pin was measured; without the row the version \
             is unsourced and criterion 6 has nothing a reviewer can check."
        )
    });
    assert!(
        provenance.contains(".json:"),
        "the capture's row for `{spec}` cites {provenance:?} instead of a `<config file>:<line>` \
         location. One version in one place is the narrowing, and a version whose recorded source \
         has been dropped is how three places came to name three different ones."
    );

    // Optional live re-measurement of that provenance, and it is fail-closed: an
    // explicit request must never degrade into a pass the way the old read did.
    if let Some(live) = std::env::var_os(LIVE_PLUGIN_CONFIG_ENV) {
        let live = PathBuf::from(live);
        let text = std::fs::read_to_string(&live).unwrap_or_else(|error| {
            panic!(
                "{LIVE_PLUGIN_CONFIG_ENV}={} could not be read ({error}). A live check that was \
                 asked for must fail, not skip.",
                live.display()
            )
        });
        assert!(
            text.contains(&spec),
            "{} does not pin {spec}, so this host's live configuration and the committed capture \
             disagree about which kiro-auth release criterion 6 is about",
            live.display()
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
            "SKIPPED criterion_6_converges_the_plan_the_capture_and_this_test_on_one_kiro_auth_\
             version (on-disk half): {} is absent, so the installed package's own manifest was \
             NOT compared against {spec} on this host. The in-repository halves above did run.",
            installed.display()
        );
    }
}

/// Point this at a real configuration file to re-measure criterion 6's provenance.
///
/// Unset by default, because the contract is anchored in the repository. When it
/// *is* set the check is fail-closed: an unreadable path or a config pinning a
/// different release fails the test. A skip would put back the defect this
/// variable replaced.
const LIVE_PLUGIN_CONFIG_ENV: &str = "OC_PLUGIN_CONFIG";

/// The evidence cell of the capture's `Installed plugins` row for `spec`.
///
/// Returns `None` when no row names `spec`, which is itself a failure at the
/// callsite — the point is that the recorded version keeps a recorded source.
fn recorded_plugin_provenance(capture: &str, spec: &str) -> Option<String> {
    let plugin_cell = format!("`{spec}`");
    capture
        .lines()
        .filter(|line| line.starts_with('|'))
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>()
        })
        .find(|cells| cells.len() >= 2 && cells[0] == plugin_cell)
        .map(|cells| cells[1].to_owned())
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
            .contains(&zuno_plugin::HookName::ChatHeaders),
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
    // The provider.info id is deliberately foreign, so this can only pass through
    // Kiro's first arm: `input?.model?.providerID`. This is a real installed plugin,
    // not a hand-built JSON assertion over the projection helper.
    assert_eq!(
        model_only.headers.get(KIRO_REQUEST_KIND_HEADER),
        Some(&"compaction".to_owned()),
        "the real kiro hook must match the SDK spelling `model.providerID` without falling back \
         to provider.info.id; injected: {:?}",
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
