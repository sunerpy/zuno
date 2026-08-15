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
//!
//! # `${VAR}` expansion, on the same evidence standard
//!
//! Todo 111's defect sat one step later: whichever rung won, the URL was dialled
//! **literally**, so `http://${PROBE_HOST}/v1` was handed to the transport with the
//! braces still in it. `resolveSDK` expands the chosen rung against the process
//! environment (`:1712-1715`), and the two tests here that parameterise a host prove
//! it the only way that matters — the substituted address is a real loopback port and
//! the mock either saw the request or it did not.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use oc_testkit::{MockProvider, MockResponse, Scenario, ScriptedEnv};

/// A recorded tool-free text completion — the smallest thing a turn can complete on.
const CASSETTE: &str = "openai-chat/streams-text";

/// Everything here is loopback, so exceeding this is a hang rather than slowness.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// A port on loopback that nothing listens on, used as a decoy endpoint.
///
/// Port 1 is privileged and unbound in every environment this suite runs in, so a
/// request routed here cannot be silently answered by something else.
const DEAD_ENDPOINT: &str = "http://127.0.0.1:1/v1";

/// The variable the placeholder tests parameterise the gateway's authority with.
///
/// Deliberately not an `OPENCODE_*` name: expansion reads the whole resolved
/// environment, exactly as `resolveSDK` reads `env.all()`, so a test that could only
/// pass through a name this program already knows would be proving the wrong thing.
const PROBE_HOST: &str = "PROBE_HOST";

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
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

fn variables(
    env: &ScriptedEnv,
    config: String,
    extra: &[(&str, &str)],
) -> BTreeMap<String, String> {
    let mut variables = env
        .env_vars()
        .into_iter()
        .map(|(key, value)| (oc_paths::env::accepted_env_name(&key).to_owned(), value))
        .collect::<BTreeMap<_, _>>();
    variables.extend(
        extra
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("ZUNO_PURE".to_owned(), "1".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), "{}".to_owned()),
        // No `ZUNO_MODELS_PATH` and no fetch: the config fully specifies
        // `test/test-model`, so nothing but the config may supply the endpoint.
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("OPENCODE_CONFIG_CONTENT".to_owned(), config),
    ]);
    variables
}

/// Launch the real binary against the real config and wait for it, bounded.
async fn run_prompt(env: &ScriptedEnv, config: String) -> Output {
    run_prompt_with(env, config, &[]).await
}

/// The same launch, with `extra` added to the child's environment.
///
/// `env_clear` means a placeholder's variable can only reach the child through this
/// argument, which is the same route a user's shell export takes: the process
/// environment `oc_paths::Env::from_process` snapshots. Nothing here injects the value
/// into the config or into a name the program already reads.
async fn run_prompt_with(env: &ScriptedEnv, config: String, extra: &[(&str, &str)]) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", "hello"])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, config, extra));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the run must finish inside its budget")
        .expect("launch zuno run")
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

async fn empty_turn_mock() -> MockProvider {
    let finish = serde_json::json!({
        "id": "chatcmpl-empty-turn",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "test-model",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    });
    let scenario = Scenario::new("empty-turn")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded title completion loads")
        .respond(MockResponse::authored(
            200,
            "text/event-stream",
            format!("data: {finish}\n\ndata: [DONE]\n\n"),
            "FU-8B requires a complete provider stream that contains no assistant part",
        ));
    MockProvider::start(vec![scenario])
        .await
        .expect("empty-turn provider binds loopback")
}

fn describe(output: &Output) -> String {
    format!("status: {:?}\n{}", output.status, combined(output))
}

/// Everything the user actually reads, whichever stream it left on.
fn combined(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_assistant_response_exits_non_zero_and_names_the_provider() {
    if oc_testkit::recordings_root_or_skip(
        "an_empty_assistant_response_exits_non_zero_and_names_the_provider",
        "the recorded title request before an empty response was NOT tested",
    )
    .is_none()
    {
        return;
    }
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = empty_turn_mock().await;
    let base_url = format!("{}/v1", provider.base_url());

    let output = run_prompt(&env, provider_config(None, None, Some(&base_url))).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert_eq!(captured.len(), 2, "{}", describe(&output));
    assert!(
        !output.status.success(),
        "a zero-part assistant response must not exit zero\n{}",
        describe(&output)
    );
    let diagnostic = combined(&output);
    assert!(diagnostic.contains("provider `test`"), "{diagnostic}");
    assert!(diagnostic.contains("empty"), "{diagnostic}");
}

/// The product, with no crutch: an endpoint in `options.baseURL` and nowhere else.
///
/// This is the exact shape the upstream docs show for a private OpenAI-compatible
/// gateway, and the shape todo 88's frozen perf workload emits. Before the fix the
/// binary declined the provider without opening a socket, so `captured` was empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn options_base_url_alone_reaches_the_endpoint() {
    if oc_testkit::recordings_root_or_skip(
        "options_base_url_alone_reaches_the_endpoint",
        "recorded endpoint dispatch was NOT tested",
    )
    .is_none()
    {
        return;
    }
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
    if oc_testkit::recordings_root_or_skip(
        "endpoint_wins_over_base_url_when_both_are_set",
        "recorded endpoint precedence was NOT tested",
    )
    .is_none()
    {
        return;
    }
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
    if oc_testkit::recordings_root_or_skip(
        "a_catalog_supplied_api_url_still_reaches_the_endpoint",
        "recorded catalog endpoint dispatch was NOT tested",
    )
    .is_none()
    {
        return;
    }
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

/// A `${VAR}` in the catalog's `api.url` is dialled at the substituted authority.
///
/// The user-facing shape is a regional gateway written once and pointed at whichever
/// region the environment names. `PROBE_HOST` carries the mock's real loopback
/// authority, so the request can only arrive if the placeholder was expanded — an
/// unexpanded `http://${PROBE_HOST}/v1` has no host a socket can be opened to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_in_the_catalog_api_url_reaches_the_substituted_host() {
    if oc_testkit::recordings_root_or_skip(
        "a_placeholder_in_the_catalog_api_url_reaches_the_substituted_host",
        "recorded catalog placeholder dispatch was NOT tested",
    )
    .is_none()
    {
        return;
    }
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = mock().await;
    let authority = provider.addr().to_string();

    let output = run_prompt_with(
        &env,
        provider_config(Some("http://${PROBE_HOST}/v1"), None, None),
        &[(PROBE_HOST, authority.as_str())],
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        !captured.is_empty(),
        "the server observed no request, so `${{PROBE_HOST}}` in the catalog's \
         `api.url` was never expanded to `{authority}`\n{}",
        describe(&output)
    );
    assert!(output.status.success(), "{}", describe(&output));
}

/// The same expansion, arriving by the other route: `provider.<id>.options.baseURL`.
///
/// Expansion applies to whichever rung todo 109's ladder chose, not only to the
/// catalog's. A fix that expanded `model.api.url` alone would pass the test above and
/// leave the documented configuration shape — endpoint in `options.baseURL` — dialling
/// braces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_arriving_via_options_base_url_reaches_the_substituted_host() {
    if oc_testkit::recordings_root_or_skip(
        "a_placeholder_arriving_via_options_base_url_reaches_the_substituted_host",
        "recorded options placeholder dispatch was NOT tested",
    )
    .is_none()
    {
        return;
    }
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = mock().await;
    let authority = provider.addr().to_string();

    let output = run_prompt_with(
        &env,
        provider_config(None, None, Some("http://${PROBE_HOST}/v1")),
        &[(PROBE_HOST, authority.as_str())],
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        !captured.is_empty(),
        "the server observed no request, so `${{PROBE_HOST}}` in \
         `provider.test.options.baseURL` was never expanded to `{authority}`\n{}",
        describe(&output)
    );
    assert!(output.status.success(), "{}", describe(&output));
}

/// A typo'd variable name reaches no endpoint at all, rather than a wrong one.
///
/// The failure half of the todo's QA pair, asserted on the only thing loopback can
/// prove: the mock is listening on the authority the *correct* spelling would have
/// produced, and it sees nothing, so the run neither reached the intended gateway by
/// accident nor collapsed the authority into something that happened to resolve. That
/// the placeholder is preserved *verbatim* rather than replaced with the empty string is
/// asserted where the resolved URL is observable, by
/// `turn_tests::an_unset_variable_keeps_its_placeholder_while_a_set_one_substitutes`; an
/// unset variable and an empty one both fail to dial, so this layer cannot tell them
/// apart and does not pretend to.
///
/// The user also learns *which* name went unexpanded. Todo 111 measured that they did
/// not: the literal travelled into the failure — `ProviderError::transient` attaches the
/// transport error and reqwest's own message names the URL — but `describe_turn_failure`
/// rendered `error.to_string()` and dropped the `#[source]` chain, so the whole report
/// was `transient provider failure (status=None)`. Todo 112 walks the chain at that
/// seam, which is why this test now asserts on the message as well as on the socket.
///
/// The comparison is case-insensitive because the authority reaches the message through
/// URL parsing, and the `url` crate lowercases a host: `${PROBE_HOST}` is reported as
/// `${probe_host}`. The name is legible and the fold is not something this seam can undo
/// — the case is gone before the error exists. Asserting case-insensitively also keeps
/// the test honest if a later change carries the config's verbatim spelling instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unset_variable_reaches_no_endpoint_rather_than_a_collapsed_one() {
    if oc_testkit::recordings_root_or_skip(
        "an_unset_variable_reaches_no_endpoint_rather_than_a_collapsed_one",
        "recorded unresolved-placeholder behavior was NOT tested",
    )
    .is_none()
    {
        return;
    }
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = mock().await;
    let authority = provider.addr().to_string();

    // `PROBE_HOST` is deliberately not exported: the config names it, nothing sets it.
    let output = run_prompt(
        &env,
        provider_config(None, None, Some("http://${PROBE_HOST}/v1")),
    )
    .await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    assert!(
        !output.status.success(),
        "a base URL naming an unset variable cannot be dialled and must not report \
         success\n{}",
        describe(&output)
    );
    assert!(
        captured.is_empty(),
        "the server on `{authority}` observed a request even though nothing set \
         `PROBE_HOST`, so the unexpanded authority resolved to something\n{}",
        describe(&output)
    );
    let message = combined(&output);
    assert!(
        message
            .to_lowercase()
            .contains(&format!("${{{}}}", PROBE_HOST.to_lowercase())),
        "the failure never named the unexpanded `${{{PROBE_HOST}}}`, so the user cannot \
         tell a misspelled variable from a dead gateway\n{message}"
    );
}

/// An endpoint nothing answers on names the URL it could not reach.
///
/// The QA scenario this todo exists for: a user typos a hostname or a port and the
/// message has to name the host, because every connection-level failure — wrong host,
/// dead port, TLS refusal, unexpanded placeholder — is otherwise the same seven words.
/// No mock is started: the whole point is that nothing is listening.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_endpoint_names_the_url_it_could_not_reach() {
    let env = ScriptedEnv::new().expect("isolated environment");

    let output = run_prompt(&env, provider_config(None, None, Some(DEAD_ENDPOINT))).await;

    assert!(
        !output.status.success(),
        "a dial that never connected must not report success\n{}",
        describe(&output)
    );
    let message = combined(&output);
    for needle in ["127.0.0.1:1", "/v1/"] {
        assert!(
            message.contains(needle),
            "the failure did not name `{needle}`, so it cannot be told apart from any \
             other transport failure\n{message}"
        );
    }
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
    let message = combined(&output);
    for needle in ["baseURL", "provider.test.options"] {
        assert!(
            message.contains(needle),
            "the refusal must name `{needle}` so the user knows which key to set; \
             got:\n{message}"
        );
    }
}
