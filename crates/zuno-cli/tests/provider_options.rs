//! What a provider's own `options` block reaches, proved by a server that observed it.
//!
//! Todo 110's defect, measured before it was fixed: `model_spec` forwarded only
//! `model.options`, and `provider.<id>.options` was read at exactly one place — the
//! endpoint ladder — so every other provider-level option was dropped on the floor.
//! `apiKey` was one of them. A config carrying the endpoint *and* the key together in
//! `provider.test.options`, which is the shape every upstream doc page shows, produced
//! this on the listener:
//!
//! ```text
//! AUTH=None
//! AUTH=None
//! ```
//!
//! The run still exited 0, because [`MockProvider`] does not check `Authorization` and
//! a cassette drops auth headers before matching. Against any real gateway it is a 401
//! for a correctly-configured user — which is why every test here asserts on the header
//! the server *received* rather than on an exit code or an error string.
//!
//! # The precedence under test
//!
//! Ported from `packages/opencode/src/provider/provider.ts`:
//!
//! - `:1676` — `const options = { ...provider.options }`. The SDK option bag is
//!   **seeded from the provider**, and model-level options are overlaid on top of it
//!   (`:1497` merges the two deep, model winning).
//! - `:1719` — `if (options["apiKey"] === undefined && provider.key) options["apiKey"] =
//!   provider.key`. A config-supplied key is **primary**; the stored credential is the
//!   **fallback**. Ours had that inverted: the credential was the only source.
//!
//! # Why `extraBody` is the option these tests plant
//!
//! It is the one forwarded option whose effect is visible on the wire: it becomes
//! request-body keys (`zuno-provider-compatible/src/provider.rs:185`). Asserting that a
//! provider-level `extraBody` key appears in the body the server parsed proves the seed
//! exists, and asserting that a model-level key of the same name replaced it while its
//! sibling survived proves the overlay is deep and that the model wins. A test on
//! `Spec::options` alone could not tell a forwarded option from an inert one.
//!
//! # Todo 109's invariant, still pinned
//!
//! `endpoint` and `baseURL` travel as [`Spec::base_url`] and must never appear in the
//! forwarded option bag. That is asserted at the unit layer (`turn_tests.rs`), because
//! it is a negative about a bag no wire field reads today; the wire test here is the
//! positive half — a non-endpoint provider option does reach the request.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use zuno_testkit::{MockProvider, MockResponse, Scenario, ScriptedEnv, trusted_platform_config};

/// A recorded tool-free text completion — the smallest thing a turn can complete on.
const CASSETTE: &str = "openai-chat/streams-text";

/// Everything here is loopback, so exceeding this is a hang rather than slowness.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// The key a config puts in `provider.test.options.apiKey`.
const OPTIONS_KEY: &str = "sk-from-options";

/// The key the auth store holds for `test`.
const STORED_KEY: &str = "sk-from-the-auth-store";

/// A key planted where a leak would be unmistakable.
///
/// Deliberately loud: a partial match, a truncation or a case change all still contain
/// `SUPERSECRET`, so a scrub that only half worked cannot pass by looking tidy.
const ECHOED_KEY: &str = "sk-SUPERSECRET-DO-NOT-ECHO";

/// A 401 body wording its rejection the way real gateways do: by quoting the key.
fn echoed_key_body() -> Vec<u8> {
    serde_json::json!({
        "error": {"message": format!("Incorrect API key provided: {ECHOED_KEY}")}
    })
    .to_string()
    .into_bytes()
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

/// Where a key may be planted, so a test names its case instead of two bare `Option`s.
#[derive(Debug, Clone, Copy)]
struct Keys {
    /// `provider.test.options.apiKey`.
    options: Option<&'static str>,
    /// The auth store's entry for `test`.
    stored: Option<&'static str>,
}

/// The auth-store document `ZUNO_AUTH_CONTENT` carries.
fn auth_content(stored: Option<&str>) -> String {
    match stored {
        Some(key) => serde_json::json!({"test": {"type": "api", "key": key}}).to_string(),
        None => "{}".to_owned(),
    }
}

/// One OpenAI-compatible provider whose endpoint is `base_url`.
///
/// `provider_extra`/`model_extra` land in the respective `options.extraBody`, which is
/// the only forwarded option with a visible wire effect. Nothing here sends a top-level
/// `api` key: it is the same URL by another name, and it was todo 109's camouflage.
fn provider_config(
    base_url: &str,
    keys: Keys,
    provider_options: serde_json::Map<String, serde_json::Value>,
    model_options: serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut options = provider_options;
    options.insert("baseURL".to_owned(), serde_json::json!(base_url));
    if let Some(key) = keys.options {
        options.insert("apiKey".to_owned(), serde_json::json!(key));
    }

    trusted_platform_config(serde_json::json!({
        "formatter": false,
        "lsp": false,
        "provider": {
            "test": {
                "name": "Test",
                "id": "test",
                "env": [],
                "transport": "openai-compatible",
                "options": serde_json::Value::Object(options),
                "models": {
                    "test-model": {
                        "id": "test-model",
                        "name": "Test Model",
                        "tool_call": true,
                        "release_date": "2025-01-01",
                        "limit": { "context": 100_000, "output": 10_000 },
                        "cost": { "input": 0, "output": 0 },
                        "options": serde_json::Value::Object(model_options)
                    }
                }
            }
        }
    }))
    .to_string()
}

fn variables(env: &ScriptedEnv, config: String, stored: Option<&str>) -> BTreeMap<String, String> {
    let mut variables = env.env_vars();
    variables.extend([
        ("NO_COLOR".to_owned(), "1".to_owned()),
        ("TERM".to_owned(), "dumb".to_owned()),
        ("ZUNO_AUTH_CONTENT".to_owned(), auth_content(stored)),
        // The config fully specifies `test/test-model`, so nothing but the config may
        // supply the endpoint or the key.
        ("ZUNO_DISABLE_MODELS_FETCH".to_owned(), "true".to_owned()),
        ("ZUNO_CONFIG_CONTENT".to_owned(), config),
    ]);
    variables
}

/// Launch the real binary against the real config and wait for it, bounded.
async fn run_prompt(env: &ScriptedEnv, config: String, stored: Option<&str>) -> Output {
    let mut command = tokio::process::Command::new(binary());
    command
        .args(["run", "--model", "test/test-model", "hello"])
        .current_dir(env.working_dir())
        .env_clear()
        .envs(variables(env, config, stored));
    tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("the run must finish inside its budget")
        .expect("launch zuno run")
}

/// A mock replaying two tool-free completions: the title prelude and the turn.
async fn mock() -> MockProvider {
    let scenario = Scenario::new("options-probe")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded text completion loads")
        .from_oracle_cassette(CASSETTE)
        .expect("the recorded text completion loads twice");
    MockProvider::start(vec![scenario])
        .await
        .expect("mock provider binds loopback")
}

/// A mock that rejects everything, so the auth failure path can be observed.
async fn unauthorized_mock() -> MockProvider {
    rejecting_mock(br#"{"error":{"message":"missing credentials"}}"#.to_vec()).await
}

/// The same rejection, with `body` as the vendor's own error text.
///
/// Parameterised so one test can plant a body that echoes the rejected key — the shape
/// real gateways use (`Incorrect API key provided: sk-…`) and the one that turns a cause
/// chain into a disclosure.
async fn rejecting_mock(body: Vec<u8>) -> MockProvider {
    let reject = || {
        MockResponse::authored(
            401,
            "application/json",
            body.clone(),
            "todo 110: the keyless failure path needs a gateway that checks auth",
        )
    };
    let scenario = Scenario::new("unauthorized")
        .respond(reject())
        .respond(reject());
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

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Run one turn and report every `Authorization` the server observed.
///
/// Every captured request, not just the first: the pre-fix measurement was `AUTH=None`
/// **twice** — the title prelude and the turn — and a fix that authenticated only one of
/// them would leave the other unauthenticated against a real gateway.
async fn observed_authorization(
    keys: Keys,
    provider_options: serde_json::Map<String, serde_json::Value>,
    model_options: serde_json::Map<String, serde_json::Value>,
) -> Option<(Vec<Option<String>>, Vec<serde_json::Value>, Output)> {
    zuno_testkit::recordings_root_or_skip(
        "provider_options::observed_authorization",
        "recorded provider option propagation was NOT tested",
    )?;
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = mock().await;
    let base_url = format!("{}/v1", provider.base_url());
    let config = provider_config(&base_url, keys, provider_options, model_options);

    let output = run_prompt(&env, config, keys.stored).await;
    let captured = provider.captured().await;
    provider.shutdown().await;

    let authorizations = captured
        .iter()
        .map(|request| request.header("authorization").map(str::to_owned))
        .collect();
    let bodies = captured
        .iter()
        .map(|request| request.json().unwrap_or(serde_json::Value::Null))
        .collect();
    Some((authorizations, bodies, output))
}

fn no_options() -> serde_json::Map<String, serde_json::Value> {
    serde_json::Map::new()
}

/// The measured defect: a key in `provider.<id>.options.apiKey` and nowhere else.
///
/// Before the fix the server logged `AUTH=None` twice and the run exited 0 anyway,
/// because the mock does not check auth. This asserts on the header the server received,
/// so "it exited 0" cannot stand in for "it authenticated".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_options_api_key_alone_reaches_the_authorization_header() {
    let keys = Keys {
        options: Some(OPTIONS_KEY),
        stored: None,
    };
    let Some((authorizations, _, output)) =
        observed_authorization(keys, no_options(), no_options()).await
    else {
        return;
    };

    assert!(
        !authorizations.is_empty(),
        "the server observed no request at all\n{}",
        describe(&output)
    );
    let expected = format!("Bearer {OPTIONS_KEY}");
    for observed in &authorizations {
        assert_eq!(
            observed.as_deref(),
            Some(expected.as_str()),
            "`provider.test.options.apiKey` never reached the wire; the server saw \
             {observed:?} on one of {} requests\n{}",
            authorizations.len(),
            describe(&output)
        );
    }
    assert!(output.status.success(), "{}", describe(&output));
}

/// The fallback, unbroken: no `options.apiKey`, so the stored credential authenticates.
///
/// `:1719` makes the credential the fallback, not the loser. A fix that only read the
/// option would break every user who authenticated with `opencode auth login`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_credential_still_authenticates_when_no_option_names_a_key() {
    let keys = Keys {
        options: None,
        stored: Some(STORED_KEY),
    };
    let Some((authorizations, _, output)) =
        observed_authorization(keys, no_options(), no_options()).await
    else {
        return;
    };

    assert!(
        !authorizations.is_empty(),
        "the server observed no request at all\n{}",
        describe(&output)
    );
    let expected = format!("Bearer {STORED_KEY}");
    for observed in &authorizations {
        assert_eq!(
            observed.as_deref(),
            Some(expected.as_str()),
            "the stored credential stopped authenticating\n{}",
            describe(&output)
        );
    }
    assert!(output.status.success(), "{}", describe(&output));
}

/// `options.apiKey` is primary — `:1719` consults the credential only when it is absent.
///
/// The two keys are deliberately different strings, so the assertion distinguishes
/// which one was chosen rather than merely observing that *a* key arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_options_api_key_wins_over_the_stored_credential() {
    let keys = Keys {
        options: Some(OPTIONS_KEY),
        stored: Some(STORED_KEY),
    };
    let Some((authorizations, _, output)) =
        observed_authorization(keys, no_options(), no_options()).await
    else {
        return;
    };

    assert!(
        !authorizations.is_empty(),
        "the server observed no request at all\n{}",
        describe(&output)
    );
    let expected = format!("Bearer {OPTIONS_KEY}");
    for observed in &authorizations {
        assert_eq!(
            observed.as_deref(),
            Some(expected.as_str()),
            "the stored credential beat `options.apiKey`; the precedence is inverted\n{}",
            describe(&output)
        );
    }
    assert!(output.status.success(), "{}", describe(&output));
}

/// A provider-level option reaches the wire — `:1676` seeds the bag from the provider.
///
/// `extraBody` is planted on the *provider*, with the model declaring none, so the only
/// way the key can appear in the body the server parsed is if the seed exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provider_level_option_reaches_the_request_body() {
    let mut provider_options = serde_json::Map::new();
    provider_options.insert(
        "extraBody".to_owned(),
        serde_json::json!({"providerKnob": "from-the-provider"}),
    );
    let keys = Keys {
        options: Some(OPTIONS_KEY),
        stored: None,
    };
    let Some((_, bodies, output)) =
        observed_authorization(keys, provider_options, no_options()).await
    else {
        return;
    };

    assert!(
        !bodies.is_empty(),
        "the server observed no request at all\n{}",
        describe(&output)
    );
    for body in &bodies {
        assert_eq!(
            body.get("providerKnob"),
            Some(&serde_json::json!("from-the-provider")),
            "a provider-level `extraBody` key never reached the request; \
             `provider.options` is still being dropped\n{}",
            describe(&output)
        );
    }
    assert!(output.status.success(), "{}", describe(&output));
}

/// The model wins on collision, and the provider's siblings survive — `:1497`.
///
/// `shared` is set on both; `providerOnly` on the provider alone. A shallow overlay
/// would drop `providerOnly` when the model's `extraBody` replaced the whole object, and
/// a provider-wins overlay would leave `shared` reading `from-the-provider`. Both are
/// distinguishable in the body the server parsed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_level_option_overrides_the_provider_level_one_of_the_same_name() {
    let mut provider_options = serde_json::Map::new();
    provider_options.insert(
        "extraBody".to_owned(),
        serde_json::json!({"shared": "from-the-provider", "providerOnly": "kept"}),
    );
    let mut model_options = serde_json::Map::new();
    model_options.insert(
        "extraBody".to_owned(),
        serde_json::json!({"shared": "from-the-model"}),
    );
    let keys = Keys {
        options: Some(OPTIONS_KEY),
        stored: None,
    };
    let Some((_, bodies, output)) =
        observed_authorization(keys, provider_options, model_options).await
    else {
        return;
    };

    assert!(
        !bodies.is_empty(),
        "the server observed no request at all\n{}",
        describe(&output)
    );
    for body in &bodies {
        assert_eq!(
            body.get("shared"),
            Some(&serde_json::json!("from-the-model")),
            "a provider-level option overrode a model-level one of the same name\n{}",
            describe(&output)
        );
        assert_eq!(
            body.get("providerOnly"),
            Some(&serde_json::json!("kept")),
            "the overlay is not deep: the model's `extraBody` replaced the provider's \
             whole object instead of its colliding leaf\n{}",
            describe(&output)
        );
    }
    assert!(output.status.success(), "{}", describe(&output));
}

/// No key in either place fails, names where to put one, and never echoes a key.
///
/// A keyless provider is **not** refused at plan time — a local endpoint legitimately
/// has none, which `CompatibleProvider::new` documents deliberately — so the failure is
/// the gateway's 401. What this pins is that the message a user reads names the key to
/// set instead of `authentication rejected by provider test`, which names nothing
/// actionable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_key_anywhere_names_where_to_put_one_and_never_prints_a_key() {
    let env = ScriptedEnv::new().expect("isolated environment");
    let provider = unauthorized_mock().await;
    let base_url = format!("{}/v1", provider.base_url());
    let keys = Keys {
        options: None,
        stored: None,
    };
    let config = provider_config(&base_url, keys, no_options(), no_options());

    let output = run_prompt(&env, config, None).await;
    provider.shutdown().await;

    assert!(
        !output.status.success(),
        "a rejected turn must not report success\n{}",
        describe(&output)
    );
    let message = combined(&output);
    for needle in ["provider.test.options.apiKey", "auth login"] {
        assert!(
            message.contains(needle),
            "the refusal must name `{needle}` so the user knows where to put a key; \
             got:\n{message}"
        );
    }
    for secret in [OPTIONS_KEY, STORED_KEY] {
        assert!(
            !message.contains(secret),
            "the failure path echoed key material (`{secret}`):\n{message}"
        );
    }
}

/// A gateway that echoes the key it rejected still leaks nothing to the user.
///
/// Todo 110's no-key-material guarantee held because the message was *composed* from the
/// provider id: there was no channel for a secret to travel down. Todo 112 opened one —
/// rendering a failure now walks its `#[source]` chain, and the innermost link on this
/// path is the vendor's own 401 body. `Incorrect API key provided: sk-…` is how real
/// gateways word it, so the body here is that shape with a key impossible to mistake for
/// anything else.
///
/// Both key sources are exercised, because [`resolved_credential`]'s precedence means a
/// scrub seeded from only one of them would pass with the config key and leak the stored
/// one — the more sensitive of the two, since `opencode auth login` is where a real
/// vendor key lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_that_echoes_the_rejected_key_still_prints_no_key_material() {
    let sources = [
        (
            Keys {
                options: Some(ECHOED_KEY),
                stored: None,
            },
            "provider.test.options.apiKey",
        ),
        (
            Keys {
                options: None,
                stored: Some(ECHOED_KEY),
            },
            "the auth store",
        ),
    ];

    for (keys, source) in sources {
        let env = ScriptedEnv::new().expect("isolated environment");
        let provider = rejecting_mock(echoed_key_body()).await;
        let base_url = format!("{}/v1", provider.base_url());
        let config = provider_config(&base_url, keys, no_options(), no_options());

        let output = run_prompt(&env, config, keys.stored).await;
        provider.shutdown().await;

        assert!(
            !output.status.success(),
            "a rejected turn must not report success ({source})\n{}",
            describe(&output)
        );
        let message = combined(&output);
        assert!(
            !message.contains(ECHOED_KEY),
            "the key from {source} reached the terminal through the vendor's error \
             body:\n{message}"
        );
        assert!(
            message.contains("Incorrect API key provided"),
            "the vendor's wording never reached the user, so this run would pass even \
             with no scrubbing at all ({source}):\n{message}"
        );
        for needle in ["provider.test.options.apiKey", "auth login"] {
            assert!(
                message.contains(needle),
                "scrubbing cost the advice `{needle}` ({source}):\n{message}"
            );
        }
    }
}
