//! Where a provider's endpoint may live, proved by a server that observed the request.
//!
//! Todo 109's defect: `model_spec` took a base URL **only** from `model.api.url`, so a
//! provider that carries its endpoint in `provider.<id>.options.baseURL` — the shape
//! every upstream doc page shows — reached the transport with no endpoint at all and
//! was declined before anything was dialled. A green suite could not see it because
//! every seam test also sent a top-level `api` key, which is the same URL by another
//! name; the crutch was the camouflage.
//!
//! # Why these assert on captured requests and not on an error string
//!
//! "It failed differently" is what made the defect look like a transport problem for
//! five waves: with only `options.baseURL` the run said `unrecoverable provider
//! failure`, and adding a top-level `api` at a *dead* port said `transient provider
//! failure` — two error strings, both failures, neither proving a socket was opened.
//! The only claim worth making is that a real server on loopback saw a real request,
//! so each test here starts [`MockProvider`] and asserts on
//! [`MockProvider::captured`].
//!
//! # The precedence under test
//!
//! Ported from `packages/opencode/src/provider/provider.ts` — `resolveSDK` at
//! `:1698-1700` (`options.baseURL`, when a non-empty string, beats `model.api.url`)
//! and the bedrock loader at `:355-358` (`options.endpoint ?? options.baseURL`):
//!
//! 1. `provider.<id>.options.endpoint`
//! 2. `provider.<id>.options.baseURL`
//! 3. the catalog's `model.api.url` — the rung a top-level `api` feeds
//!
//! [`endpoint_wins_over_base_url_when_both_are_set`] is the one that needs a decoy: it
//! points `baseURL` at a port nothing listens on, so the request can only be observed
//! if `endpoint` was preferred.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use oc_testkit::{MockProvider, Scenario, ScriptedEnv};

/// A recorded tool-free text completion — the smallest thing a turn can complete on.
const CASSETTE: &str = "openai-chat/streams-text";

/// Everything here is loopback, so exceeding this is a hang rather than slowness.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// A port on loopback that nothing listens on, used as a decoy endpoint.
///
/// Port 1 is privileged and unbound in every environment this suite runs in, so a
/// request routed here cannot be silently answered by something else.
const DEAD_ENDPOINT: &str = "http://127.0.0.1:1/v1";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencode-rust"))
}

/// One OpenAI-compatible provider, with the endpoint placed wherever the caller says.
///
/// `endpoint`/`base_url` land in `provider.test.options`; `api` is the top-level key.
/// Every one of them is `Option`, because the point of these tests is which
/// combinations reach a socket — including the combination of none at all.
fn provider_config(api: Option<&str>, endpoint: Option<&str>, base_url: Option<&str>) -> String {
    let mut options = serde_json::Map::new();
    options.insert("apiKey".to_owned(), serde_json::json!("test-key"));
    if let Some(endpoint) = endpoint {
        options.insert("endpoint".to_owned(), serde_json::json!(endpoint));
    }
    if let Some(base_url) = base_url {
        options.insert("baseURL".to_owned(), serde_json::json!(base_url));
    }

    let mut provider = serde_json::Map::new();
    provider.insert("name".to_owned(), serde_json::json!("Test"));
    provider.insert("id".to_owned(), serde_json::json!("test"));
    provider.insert("env".to_owned(), serde_json::json!([]));
    provider.insert(
        "npm".to_owned(),
        serde_json::json!("@ai-sdk/openai-compatible"),
    );
    if let Some(api) = api {
        provider.insert("api".to_owned(), serde_json::json!(api));
    }
    provider.insert(
        "models".to_owned(),
        serde_json::json!({
            "test-model": {
                "id": "test-model",
                "name": "Test Model",
                "tool_call": true,
                "release_date": "2025-01-01",
                "limit": { "context": 100_000, "output": 10_000 },
                "cost": { "input": 0, "output": 0 },
                "options": {}
            }
        }),
    );
    provider.insert("options".to_owned(), serde_json::Value::Object(options));

    serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": { "test": serde_json::Value::Object(provider) }
    })
    .to_string()
}

fn variables(env: &ScriptedEnv, config: String) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("OPENCODE_PURE".to_owned(), "1".to_owned()),
        ("OPENCODE_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        // No `OPENCODE_MODELS_PATH` and no fetch: the config fully specifies
        // `test/test-model`, so nothing but the config may supply the endpoint.
        (
            "OPENCODE_DISABLE_MODELS_FETCH".to_owned(),
            "true".to_owned(),
        ),
        ("OPENCODE_CONFIG_CONTENT".to_owned(), config),
    ]);
    variables
}

/// Launch the real binary against the real config and wait for it, bounded.
async fn run_prompt(env: &ScriptedEnv, config: String) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", "hello"])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, config));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the run must finish inside its budget")
        .expect("launch opencode-rust run")
}

/// A mock provider replaying two tool-free completions: the title prelude and the turn.
async fn mock() -> MockProvider {
    let scenario = Scenario::new("endpoint-probe")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded text completion loads")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded text completion loads twice");
    MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback")
}

fn describe(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The product, with no crutch: an endpoint in `options.baseURL` and nowhere else.
///
/// This is the exact shape the upstream docs show for a private OpenAI-compatible
/// gateway, and the shape todo 88's frozen perf workload emits. Before the fix the
/// binary declined the provider without opening a socket, so `captured` was empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn options_base_url_alone_reaches_the_endpoint() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = mock().await;
    let base_url = format!("{}/v1", provider.base_url());

    let output = run_prompt(&env, provider_config(None, None, Some(&base_url))).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        !captured.is_empty(),
        "the server observed no request, so `provider.test.options.baseURL` never \
         became the endpoint\n{}",
        describe(&output)
    );
    assert!(output.status.success(), "{}", describe(&output));
}

/// `options.endpoint` beats `options.baseURL` — `provider.ts:355-358`.
///
/// `baseURL` is a dead port, so the only way the mock sees anything is if `endpoint`
/// was preferred. Reversing the precedence makes this test fail rather than pass
/// slower, which is the point: both keys naming a live server would prove nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_wins_over_base_url_when_both_are_set() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = mock().await;
    let endpoint = format!("{}/v1", provider.base_url());

    let output = run_prompt(
        &env,
        provider_config(None, Some(&endpoint), Some(DEAD_ENDPOINT)),
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        !captured.is_empty(),
        "the server observed no request, so `options.baseURL` ({DEAD_ENDPOINT}) was \
         preferred over `options.endpoint`\n{}",
        describe(&output)
    );
    assert!(output.status.success(), "{}", describe(&output));
}

/// The rung this todo must not break: a catalog-supplied `api.url` still works.
///
/// Here the only endpoint is the top-level `api` key, which the merge ladder
/// (`provider.ts:1455`) resolves into `model.api.url`. Added precedence, not a
/// replacement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalog_supplied_api_url_still_reaches_the_endpoint() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = mock().await;
    let api = format!("{}/v1", provider.base_url());

    let output = run_prompt(&env, provider_config(Some(&api), None, None)).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        !captured.is_empty(),
        "the server observed no request, so `provider.test.api` stopped feeding \
         `model.api.url`\n{}",
        describe(&output)
    );
    assert!(output.status.success(), "{}", describe(&output));
}

/// No endpoint in either place fails fast and names the key to set.
///
/// The pre-fix message was `unrecoverable provider failure (status=None)`, which names
/// nothing a user can act on. A run that cannot know where to dial must say so before
/// it dials, and must say which key supplies it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_endpoint_anywhere_fails_fast_and_names_the_key() {
    let env = ScriptedEnv::new().expect("isolated environment");
    // No mock at all: nothing may be dialled, so nothing needs to listen.
    let output = run_prompt(&env, provider_config(None, None, None)).await;

    assert!(
        !output.status.success(),
        "a provider with no endpoint anywhere must not report success\n{}",
        describe(&output)
    );
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for needle in ["baseURL", "provider.test.options"] {
        assert!(
            message.contains(needle),
            "the refusal must name `{needle}` so the user knows which key to set; \
             got:\n{message}"
        );
    }
}
