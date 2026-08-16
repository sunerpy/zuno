use std::path::Path;

use zuno_plugin::{JsHostBuilder, JsPluginInput, JsPluginSpec, discover_runtime};
use zuno_server::api::ApiState;
use zuno_server::{CompatV1State, ServerBuilder, ServerConfig, compat_v1_router};

const SDK: &str = "/config/.bun/install/cache/@opencode-ai/sdk@1.15.13@@registry.npmmirror.com@@@1";

#[tokio::test]
async fn plugin_input_client_provider_list_observes_the_production_sdk_projection() {
    // Given
    let sdk = Path::new(SDK);
    if !sdk.join("dist/index.js").is_file() {
        eprintln!(
            "SKIPPED plugin_input_client_provider_list_observes_the_production_sdk_projection: \
             {SDK} is absent"
        );
        return;
    }
    let temp = tempfile::tempdir().expect("temporary generated-client fixture");
    let directory = temp.path().join("repo");
    std::fs::create_dir(&directory).expect("fixture repository directory");
    let models = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zuno-llm/tests/fixtures/models-dev-pinned.json")
        .canonicalize()
        .expect("pinned models.dev fixture");
    let env = zuno_paths::Env::empty()
        .with("HOME", temp.path().to_string_lossy().into_owned())
        .with("ZUNO_TEST_HOME", temp.path().to_string_lossy().into_owned())
        .with("ZUNO_MODELS_PATH", models.to_string_lossy().into_owned())
        .with("ZUNO_DISABLE_MODELS_FETCH", "1")
        .with("DEEPSEEK_API_KEY", "probe-key");
    let state = ApiState::memory(directory.to_string_lossy())
        .expect("in-memory API state")
        .with_env(env);
    let app = ServerBuilder::new(
        ServerConfig::default().with_default_directory(directory.to_string_lossy()),
    )
    .with_routes(compat_v1_router(CompatV1State::new(), state))
    .router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind production router");
    let address = listener.local_addr().expect("production router address");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let marker = temp.path().join("provider-shape.json");
    let marker_literal = serde_json::to_string(&marker.to_string_lossy()).expect("marker literal");
    let entry = temp.path().join("generated-client-plugin.mjs");
    std::fs::write(
        &entry,
        format!(
            r#"import {{ writeFileSync }} from "node:fs";
export default {{
  id: "generated-client-fixture",
  server: async (input) => {{
    const response = await input.client.provider.list();
    const provider = response.data.all.find((candidate) => candidate.id === "deepseek");
    const model = provider.models["deepseek-reasoner"];
    writeFileSync({marker_literal}, JSON.stringify({{
      provider: {{ id: provider.id, api: provider.api, npm: provider.npm, env: provider.env }},
      model,
    }}));
    return {{}};
  }},
}};"#
        ),
    )
    .expect("write generated-client plugin");
    let runtime =
        discover_runtime(&["generated-client-fixture".to_owned()]).expect("JavaScript runtime");
    let input = JsPluginInput::new(&directory, &directory, format!("http://{address}"))
        .with_sdk_module(sdk);

    // When
    let host = JsHostBuilder::new(
        "generated-client-fixture",
        runtime,
        &JsPluginSpec::new(format!("file:{}", entry.display())),
        &entry,
        input,
    )
    .start()
    .await
    .expect("the real generated client reaches the production router");
    host.shutdown().await;
    server.abort();

    // Then
    let observed: serde_json::Value = serde_json::from_slice(
        &std::fs::read(marker).expect("plugin wrote its observed provider shape"),
    )
    .expect("decode observed provider shape");
    assert_eq!(observed["provider"]["id"], "deepseek");
    assert_eq!(observed["provider"]["api"], "https://api.deepseek.com");
    assert_eq!(observed["provider"]["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(
        observed["provider"]["env"],
        serde_json::json!(["DEEPSEEK_API_KEY"])
    );
    assert_eq!(observed["model"]["release_date"], "2025-12-01");
    assert_eq!(observed["model"]["reasoning"], true);
    assert_eq!(observed["model"]["temperature"], true);
    assert_eq!(observed["model"]["tool_call"], true);
    assert_eq!(
        observed["model"]["modalities"]["input"],
        serde_json::json!(["text"])
    );
    assert_eq!(observed["model"]["limit"]["context"], 1_000_000);
    assert_eq!(observed["model"]["cost"]["cache_read"], 0.0028);
}
