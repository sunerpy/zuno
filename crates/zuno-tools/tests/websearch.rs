//! `websearch` against a real HTTP server, and its exposure against a real tool list.
//!
//! # The failure scenario this file exists for
//!
//! [`an_unconfigured_websearch_is_absent_from_the_tool_list`] builds the tool list the
//! way todo 44's registry will and asserts `websearch` is **not in it** when no
//! provider is configured. A tool that is present and fails on every call costs its
//! schema in prompt tokens on every request and teaches the model to reason about
//! refusals; absence is the correct behaviour, and it has to be a tested predicate
//! rather than a branch someone remembers to write.
//!
//! # No test here reaches a search backend
//!
//! Every call is pointed at a `wiremock` server through
//! [`WebSearchTool::with_endpoint`]. Reaching Exa or Parallel would burn a real API
//! key and prove nothing about the transport.

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zuno_error::ToolError;
use zuno_permission::visibility::{is_tool_hidden, is_tool_visible};
use zuno_permission::{PermissionAction, Rule};
use zuno_tool::{AllowAll, DenyAll, NeverInterrupted, Tool, ToolContext, ToolOutput, Typed, erase};
use zuno_tools::webfetch::bounds::WebError;
use zuno_tools::websearch::gating::{
    ENV_ENABLE_EXA, ENV_ENABLE_PARALLEL, ENV_EXA_API_KEY, ENV_PARALLEL_API_KEY, ENV_PROVIDER,
    Provider, SearchConfig,
};
use zuno_tools::websearch::{WebSearchTool, mcp};
use zuno_tools::{WebFetchTool, web_search_enabled};

fn config(pairs: &[(&str, &str)]) -> SearchConfig {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    SearchConfig::from_lookup(|key| {
        owned
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    })
}

fn context(session_id: &str) -> ToolContext {
    ToolContext::new(
        session_id,
        "msg_1",
        "call_1",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn envelope(text: &str) -> Value {
    json!({ "result": { "content": [{ "type": "text", "text": text }] } })
}

async fn mount_search(server: &MockServer, text: &str) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(text)))
        .mount(server)
        .await;
}

async fn run(tool: WebSearchTool, args: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
    Tool::execute(&Typed(tool), args, ctx).await
}

// ---------------------------------------------------------------------------
// The failure scenario: gating by absence
// ---------------------------------------------------------------------------

/// The tool list todo 44's registry will assemble, filtered the way it will filter.
///
/// Two filters, in upstream's order: the model-conditional predicate
/// (`registry.ts:288-290`) and then permission-based hiding (`index.ts:204-219`).
fn resolve_tool_ids(provider_id: &str, search: &WebSearchTool, rules: &[Rule]) -> Vec<String> {
    let tools: Vec<Arc<dyn Tool>> = vec![erase(WebFetchTool::new()), erase(clone_of(search))];
    tools
        .iter()
        .filter(|tool| tool.id() != "websearch" || search.enabled_for(provider_id))
        .filter(|tool| is_tool_visible(tool.id(), rules))
        .map(|tool| tool.id().to_owned())
        .collect()
}

/// A second tool over the same configuration, since `WebSearchTool` owns a client and
/// is deliberately not `Clone`.
fn clone_of(tool: &WebSearchTool) -> WebSearchTool {
    WebSearchTool::with_config(tool.config().clone())
}

#[test]
fn an_unconfigured_websearch_is_absent_from_the_tool_list() {
    let search = WebSearchTool::with_config(SearchConfig::default());
    let resolved = resolve_tool_ids("openai", &search, &[]);

    assert_eq!(
        resolved,
        vec!["webfetch".to_owned()],
        "an unconfigured websearch must not be advertised at all"
    );
    assert!(
        !resolved.iter().any(|id| id == "websearch"),
        "resolved: {resolved:?}"
    );
    assert!(!search.enabled_for("openai"));
}

#[test]
fn the_same_tool_appears_once_a_provider_is_configured() {
    let search = WebSearchTool::with_config(config(&[(ENV_ENABLE_EXA, "true")]));
    assert_eq!(
        resolve_tool_ids("openai", &search, &[]),
        vec!["webfetch".to_owned(), "websearch".to_owned()]
    );
}

#[test]
fn the_hosted_provider_gets_websearch_with_no_configuration() {
    let search = WebSearchTool::with_config(SearchConfig::default());
    assert_eq!(
        resolve_tool_ids("opencode", &search, &[]),
        vec!["webfetch".to_owned(), "websearch".to_owned()]
    );
}

#[test]
fn a_blanket_deny_hides_a_configured_websearch_too() {
    // The two mechanisms are independent: gating answers "is it configured", the
    // permission layer answers "is it allowed", and either can remove the tool.
    let search = WebSearchTool::with_config(config(&[(ENV_ENABLE_PARALLEL, "1")]));
    let rules = vec![Rule {
        permission: "websearch".to_owned(),
        pattern: "*".to_owned(),
        action: PermissionAction::Deny,
    }];

    assert!(search.enabled_for("openai"), "it is configured");
    assert!(is_tool_hidden("websearch", &rules), "and it is denied");
    assert_eq!(
        resolve_tool_ids("openai", &search, &rules),
        vec!["webfetch".to_owned()]
    );
}

#[test]
fn a_narrower_deny_leaves_the_tool_visible_and_is_enforced_at_call_time() {
    let search = WebSearchTool::with_config(config(&[(ENV_ENABLE_PARALLEL, "1")]));
    let rules = vec![Rule {
        permission: "websearch".to_owned(),
        pattern: "site:internal.test *".to_owned(),
        action: PermissionAction::Deny,
    }];

    assert!(!is_tool_hidden("websearch", &rules));
    assert_eq!(
        resolve_tool_ids("openai", &search, &rules),
        vec!["webfetch".to_owned(), "websearch".to_owned()]
    );
}

#[test]
fn the_gating_predicate_is_the_registrys_and_not_a_second_copy() {
    // `enabled_for` must delegate to the shared predicate, or the registry and the
    // tool could disagree about whether the tool exists.
    for (provider, pairs) in [
        ("openai", &[][..]),
        ("openai", &[(ENV_ENABLE_EXA, "true")][..]),
        ("openai", &[(ENV_ENABLE_PARALLEL, "true")][..]),
        ("opencode", &[][..]),
        ("anthropic", &[(ENV_PROVIDER, "exa")][..]),
    ] {
        let settings = config(pairs);
        let tool = WebSearchTool::with_config(settings.clone());
        assert_eq!(
            tool.enabled_for(provider),
            web_search_enabled(provider, &settings),
            "disagreement for {provider} with {pairs:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Routing and request shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_exa_search_sends_exas_tool_name_and_arguments() {
    let server = MockServer::start().await;
    mount_search(&server, "exa results").await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let output = run(
        tool,
        json!({ "query": "rust bounded fetch" }),
        context("ses_1"),
    )
    .await
    .expect("the search succeeds");

    assert_eq!(output.output, "exa results");
    assert_eq!(output.title, "Exa Web Search: rust bounded fetch");
    assert_eq!(output.metadata["provider"], "exa");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
    let body: Value = requests[0].body_json().expect("a json body");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], mcp::EXA_TOOL);
    assert_eq!(body["params"]["arguments"]["query"], "rust bounded fetch");
    assert_eq!(body["params"]["arguments"]["numResults"], 8);
    assert_eq!(body["params"]["arguments"]["livecrawl"], "fallback");
    assert_eq!(body["params"]["arguments"]["type"], "auto");
}

#[tokio::test]
async fn a_parallel_search_sends_parallels_objective_shape_and_bearer_token() {
    let server = MockServer::start().await;
    mount_search(&server, "parallel results").await;

    let tool = WebSearchTool::with_config(config(&[
        (ENV_PROVIDER, "parallel"),
        (ENV_PARALLEL_API_KEY, "par-secret"),
    ]))
    .with_endpoint(format!("{}/mcp", server.uri()));
    let output = run(tool, json!({ "query": "bounded" }), context("ses_2"))
        .await
        .expect("the search succeeds");

    assert_eq!(output.title, "Parallel Web Search: bounded");
    assert_eq!(output.metadata["provider"], "parallel");

    let requests = server.received_requests().await.expect("recorded requests");
    let body: Value = requests[0].body_json().expect("a json body");
    assert_eq!(body["params"]["name"], mcp::PARALLEL_TOOL);
    assert_eq!(body["params"]["arguments"]["objective"], "bounded");
    assert_eq!(
        body["params"]["arguments"]["search_queries"],
        json!(["bounded"])
    );
    assert_eq!(body["params"]["arguments"]["session_id"], "ses_2");

    assert_eq!(
        requests[0].headers[reqwest::header::AUTHORIZATION.as_str()]
            .to_str()
            .expect("an ascii header"),
        "Bearer par-secret"
    );
    assert!(
        requests[0].headers[reqwest::header::USER_AGENT.as_str()]
            .to_str()
            .expect("an ascii header")
            .starts_with("opencode/"),
    );
}

#[tokio::test]
async fn the_exa_key_never_appears_in_a_header() {
    // Exa's key belongs in the URL; sending it as a bearer token as well would leak
    // it to a server that never asked for it.
    let server = MockServer::start().await;
    mount_search(&server, "results").await;

    let tool = WebSearchTool::with_config(config(&[
        (ENV_PROVIDER, "exa"),
        (ENV_EXA_API_KEY, "exa-secret"),
    ]))
    .with_endpoint(format!("{}/mcp", server.uri()));
    run(tool, json!({ "query": "q" }), context("ses_3"))
        .await
        .expect("the search succeeds");

    let requests = server.received_requests().await.expect("recorded requests");
    assert!(
        !requests[0]
            .headers
            .contains_key(reqwest::header::AUTHORIZATION.as_str()),
        "exa must not be sent an Authorization header"
    );
    for (name, value) in &requests[0].headers {
        let rendered = value.to_str().unwrap_or_default();
        assert!(
            !rendered.contains("exa-secret"),
            "the key leaked into header {name}: {rendered}"
        );
    }
}

#[tokio::test]
async fn an_sse_response_is_parsed_like_a_json_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("event: message\ndata: {}\n\n", envelope("streamed")),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let output = run(tool, json!({ "query": "q" }), context("ses_4"))
        .await
        .expect("an sse body parses");

    assert_eq!(output.output, "streamed");
}

#[tokio::test]
async fn an_empty_result_reports_the_no_results_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope("   ")))
        .mount(&server)
        .await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let output = run(tool, json!({ "query": "q" }), context("ses_5"))
        .await
        .expect("an empty result is not a failure");

    assert_eq!(output.output, zuno_tools::websearch::NO_RESULTS);
}

#[tokio::test]
async fn the_session_keeps_one_backend_across_calls() {
    let unconfigured = SearchConfig::default();
    let tool = WebSearchTool::with_config(unconfigured);
    let first = tool.provider_for("ses_sticky");
    for _ in 0..8 {
        assert_eq!(tool.provider_for("ses_sticky"), first);
    }
    // And the split is not constant across sessions, or it would not be a split.
    let providers: Vec<Provider> = (0..12)
        .map(|index| tool.provider_for(&format!("ses_{index}")))
        .collect();
    assert!(
        providers.contains(&Provider::Exa) && providers.contains(&Provider::Parallel),
        "expected both backends across 12 sessions, got {providers:?}"
    );
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_search_response_is_capped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(vec![b'{'; mcp::MAX_RESPONSE_BYTES + 1]),
        )
        .mount(&server)
        .await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let error = run(tool, json!({ "query": "q" }), context("ses_6"))
        .await
        .expect_err("an oversized search response must be refused");

    let ToolError::Failed { tool, source } = error else {
        panic!("expected a classified failure, got {error:?}");
    };
    assert_eq!(tool, "websearch");
    assert!(
        matches!(
            source.downcast_ref::<WebError>(),
            Some(WebError::TooLarge { limit, .. }) if *limit == mcp::MAX_RESPONSE_BYTES
        ),
        "expected TooLarge naming the 256KiB cap, got {source}"
    );
}

#[tokio::test]
async fn a_hanging_search_backend_fails_at_the_timeout() {
    // Shortened to 1s so the suite does not wait out upstream's real 25 seconds; the
    // default is pinned by `the_search_timeout_is_upstreams_twenty_five_seconds`, so
    // the shortening cannot hide a wrong default.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(90)))
        .mount(&server)
        .await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()))
        .with_timeout(Duration::from_secs(1));

    let started = Instant::now();
    let error = run(tool, json!({ "query": "q" }), context("ses_7"))
        .await
        .expect_err("a hanging backend must fail, not hang the turn");
    let elapsed = started.elapsed();

    match error {
        ToolError::Timeout {
            tool,
            elapsed: reported,
        } => {
            assert_eq!(tool, "websearch");
            assert_eq!(reported, Duration::from_secs(1));
        }
        other => panic!("expected a typed Timeout, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_secs(1),
        "gave up early at {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "took {elapsed:?}; the stall was not bounded"
    );
}

#[tokio::test]
async fn the_default_budget_is_wired_and_not_merely_declared() {
    // Proves the default reaches the `timeout` call rather than being a constant the
    // implementation forgot to read: a tool built without `with_timeout` reports
    // upstream's 25s in its typed failure.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(90)))
        .mount(&server)
        .await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));

    let error = tokio::time::timeout(
        mcp::TIMEOUT + Duration::from_secs(10),
        run(tool, json!({ "query": "q" }), context("ses_default")),
    )
    .await
    .expect("the tool's own budget must fire before the test's outer bound")
    .expect_err("a hanging backend must fail");

    match error {
        ToolError::Timeout { elapsed, .. } => assert_eq!(elapsed, mcp::TIMEOUT),
        other => panic!("expected a typed Timeout, got {other:?}"),
    }
}

#[test]
fn the_search_timeout_is_upstreams_twenty_five_seconds() {
    assert_eq!(mcp::TIMEOUT, Duration::from_secs(25));
}

#[test]
fn the_search_cap_is_upstreams_two_hundred_and_fifty_six_kibibytes() {
    assert_eq!(mcp::MAX_RESPONSE_BYTES, 256 * 1024);
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_backend_error_page_is_a_classified_failure_not_search_results() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let error = run(tool, json!({ "query": "q" }), context("ses_8"))
        .await
        .expect_err("a 401 is not a result set");

    let ToolError::Failed { source, .. } = error else {
        panic!("expected a classified failure, got {error:?}");
    };
    assert!(
        matches!(
            source.downcast_ref::<WebError>(),
            Some(WebError::Status { status: 401, .. })
        ),
        "{source}"
    );
}

#[tokio::test]
async fn an_unparseable_response_names_the_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>gateway error</html>"))
        .mount(&server)
        .await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "parallel")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let error = run(tool, json!({ "query": "q" }), context("ses_9"))
        .await
        .expect_err("an html error page is not an mcp envelope");

    let ToolError::Failed { source, .. } = error else {
        panic!("expected a classified failure, got {error:?}");
    };
    assert!(
        matches!(
            source.downcast_ref::<WebError>(),
            Some(WebError::MalformedSearchResponse {
                provider: "parallel"
            })
        ),
        "{source}"
    );
}

#[tokio::test]
async fn an_empty_query_is_refused_before_any_request() {
    let server = MockServer::start().await;
    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));

    let error = run(tool, json!({ "query": "   " }), context("ses_10"))
        .await
        .expect_err("a blank query must not be sent");

    assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    assert!(
        server
            .received_requests()
            .await
            .is_some_and(|requests| requests.is_empty()),
        "a refused query must not touch the network"
    );
}

#[tokio::test]
async fn the_permission_gate_is_consulted_before_the_request() {
    let server = MockServer::start().await;
    mount_search(&server, "should never be reached").await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let ctx = ToolContext::new(
        "ses_11",
        "msg_1",
        "call_1",
        "build",
        Arc::new(DenyAll),
        Arc::new(NeverInterrupted),
    );

    let error = run(tool, json!({ "query": "q" }), ctx)
        .await
        .expect_err("a denied search must not run");

    assert!(matches!(error, ToolError::Denied { .. }), "{error:?}");
    assert!(
        server
            .received_requests()
            .await
            .is_some_and(|requests| requests.is_empty()),
        "a denied search must not touch the network"
    );
}
