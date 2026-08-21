//! Hosted MCP adapter and native web-search exposure tests.

use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wiremock::matchers::{body_partial_json, method, path};
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
use zuno_tools::websearch::{ID, NO_RESULTS, WebSearchTool, mcp};
use zuno_tools::{WebFetchTool, web_search_enabled};

fn config(pairs: &[(&str, &str)]) -> SearchConfig {
    SearchConfig::from_lookup(|key| {
        pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).to_owned())
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

fn resolve_tool_ids(search: &WebSearchTool, rules: &[Rule]) -> Vec<String> {
    let tools: Vec<Arc<dyn Tool>> = vec![erase(WebFetchTool::new()), erase(search.clone())];
    tools
        .iter()
        .filter(|tool| tool.id() != ID || search.enabled_for("ignored"))
        .filter(|tool| is_tool_visible(tool.id(), rules))
        .map(|tool| tool.id().to_owned())
        .collect()
}

#[test]
fn unconfigured_search_is_absent_and_native_configuration_exposes_it() {
    let absent = WebSearchTool::with_config(SearchConfig::default());
    assert_eq!(resolve_tool_ids(&absent, &[]), vec!["webfetch"]);
    assert!(!web_search_enabled(absent.config()));

    for settings in [
        config(&[(ENV_ENABLE_EXA, "true")]),
        config(&[(ENV_ENABLE_PARALLEL, "true")]),
        config(&[(ENV_EXA_API_KEY, "key")]),
        config(&[(ENV_PROVIDER, "parallel")]),
    ] {
        let search = WebSearchTool::with_config(settings);
        assert_eq!(
            resolve_tool_ids(&search, &[]),
            vec!["webfetch".to_owned(), ID.to_owned()]
        );
    }
}

#[test]
fn a_full_deny_hides_search_but_a_narrow_rule_keeps_it_visible() {
    let search = WebSearchTool::with_config(config(&[(ENV_ENABLE_EXA, "true")]));
    let denied = [Rule {
        permission: ID.to_owned(),
        pattern: "*".to_owned(),
        action: PermissionAction::Deny,
    }];
    assert!(is_tool_hidden(ID, &denied));
    assert_eq!(resolve_tool_ids(&search, &denied), vec!["webfetch"]);

    let narrow = [Rule {
        permission: ID.to_owned(),
        pattern: "site:internal.test *".to_owned(),
        action: PermissionAction::Deny,
    }];
    assert!(!is_tool_hidden(ID, &narrow));
    assert_eq!(
        resolve_tool_ids(&search, &narrow),
        vec!["webfetch".to_owned(), ID.to_owned()]
    );
}

#[tokio::test]
async fn exa_receives_one_request_per_distinct_query_with_profile_owned_limits() {
    let server = MockServer::start().await;
    mount_search(&server, "[Rust](https://www.rust-lang.org/) search result").await;

    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()));
    let output = run(
        tool,
        json!({ "queries": ["rust bounded fetch", "rust bounded fetch"] }),
        context("ses_1"),
    )
    .await
    .expect("search succeeds");

    assert_eq!(output.title, "Web search: rust bounded fetch");
    assert!(output.output.contains("Rust"));
    assert_eq!(
        output.metadata["sources"],
        json!([{ "url": "https://www.rust-lang.org/", "title": "Rust" }])
    );

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let body: Value = requests[0].body_json().expect("JSON body");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], mcp::EXA_TOOL);
    assert_eq!(body["params"]["arguments"]["query"], "rust bounded fetch");
    assert_eq!(body["params"]["arguments"]["numResults"], 8);
    assert_eq!(body["params"]["arguments"]["livecrawl"], "fallback");
    assert_eq!(body["params"]["arguments"]["type"], "auto");
}

#[tokio::test]
async fn parallel_receives_its_native_request_and_bearer_token() {
    let server = MockServer::start().await;
    mount_search(&server, "Result https://parallel.test/item").await;

    let tool = WebSearchTool::with_config(config(&[
        (ENV_PROVIDER, "parallel"),
        (ENV_PARALLEL_API_KEY, "par-secret"),
    ]))
    .with_endpoint(format!("{}/mcp", server.uri()));
    let output = run(tool, json!({ "queries": ["bounded"] }), context("ses_2"))
        .await
        .expect("search succeeds");
    assert!(output.output.contains("parallel.test"));

    let requests = server.received_requests().await.expect("requests");
    let body: Value = requests[0].body_json().expect("JSON body");
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
            .expect("ASCII header"),
        "Bearer par-secret"
    );
    assert!(
        requests[0].headers[reqwest::header::USER_AGENT.as_str()]
            .to_str()
            .expect("ASCII header")
            .starts_with("zuno/")
    );
}

#[tokio::test]
async fn exa_key_is_only_attached_to_the_endpoint() {
    let server = MockServer::start().await;
    mount_search(&server, "result").await;
    let tool = WebSearchTool::with_config(config(&[
        (ENV_PROVIDER, "exa"),
        (ENV_EXA_API_KEY, "exa-secret"),
    ]))
    .with_endpoint(format!("{}/mcp", server.uri()));
    run(tool, json!({ "queries": ["q"] }), context("ses_3"))
        .await
        .expect("search succeeds");

    let requests = server.received_requests().await.expect("requests");
    assert!(
        !requests[0]
            .headers
            .contains_key(reqwest::header::AUTHORIZATION.as_str())
    );
}

#[tokio::test]
async fn an_sse_response_is_normalized_like_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                "event: message\ndata: {}\n\n",
                envelope("streamed https://sse.test/result")
            ),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let output = run(
        WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
            .with_endpoint(format!("{}/mcp", server.uri())),
        json!({ "queries": ["q"] }),
        context("ses_4"),
    )
    .await
    .expect("SSE parses");
    assert!(output.output.contains("streamed"));
    assert_eq!(
        output.metadata["sources"],
        json!([{ "url": "https://sse.test/result" }])
    );
}

#[tokio::test]
async fn an_empty_provider_result_has_an_explicit_empty_state() {
    let server = MockServer::start().await;
    mount_search(&server, "   ").await;
    let output = run(
        WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
            .with_endpoint(format!("{}/mcp", server.uri())),
        json!({ "queries": ["q"] }),
        context("ses_5"),
    )
    .await
    .expect("empty result is successful");
    assert!(output.output.contains(NO_RESULTS));
    assert_eq!(output.metadata["sources"], json!([]));
}

#[tokio::test]
async fn an_oversized_response_is_refused_before_becoming_tool_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(vec![b'{'; mcp::MAX_RESPONSE_BYTES + 1]),
        )
        .mount(&server)
        .await;
    let error = run(
        WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
            .with_endpoint(format!("{}/mcp", server.uri())),
        json!({ "queries": ["q"] }),
        context("ses_6"),
    )
    .await
    .expect_err("oversized response");
    let ToolError::Failed { source, .. } = error else {
        panic!("expected classified failure, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<WebError>(),
        Some(WebError::TooLarge { limit, .. }) if *limit == mcp::MAX_RESPONSE_BYTES
    ));
}

#[tokio::test]
async fn a_hanging_backend_is_bounded_by_profile_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(90)))
        .mount(&server)
        .await;
    let tool = WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
        .with_endpoint(format!("{}/mcp", server.uri()))
        .with_timeout(Duration::from_millis(100));
    let started = Instant::now();
    let error = run(tool, json!({ "queries": ["q"] }), context("ses_7"))
        .await
        .expect_err("timeout");
    assert!(matches!(
        error,
        ToolError::Timeout { elapsed, .. } if elapsed == Duration::from_millis(100)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn a_failed_http_query_cancels_a_sibling_waiting_for_response_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "params": { "arguments": { "query": "fail" } }
        })))
        .respond_with(
            ResponseTemplate::new(500)
                .insert_header("retry-after", "4")
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({
            "params": { "arguments": { "query": "slow" } }
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(envelope("too late")),
        )
        .mount(&server)
        .await;

    let started = Instant::now();
    let error = run(
        WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
            .with_endpoint(format!("{}/mcp", server.uri()))
            .with_timeout(Duration::from_secs(10)),
        json!({ "queries": ["slow", "fail"] }),
        context("ses_cancel"),
    )
    .await
    .expect_err("one failed query fails the whole batch");

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the delayed sibling was not cancelled: {:?}",
        started.elapsed()
    );
    let ToolError::Transient {
        retry_after,
        source,
        ..
    } = error
    else {
        panic!("expected transient provider failure");
    };
    assert_eq!(retry_after, Some(Duration::from_secs(4)));
    assert!(matches!(
        source.downcast_ref::<WebError>(),
        Some(WebError::Status { status: 500, .. })
    ));
}

#[tokio::test]
async fn transport_and_parse_failures_remain_classified() {
    let unauthorized = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&unauthorized)
        .await;
    let error = run(
        WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
            .with_endpoint(format!("{}/mcp", unauthorized.uri())),
        json!({ "queries": ["q"] }),
        context("ses_8"),
    )
    .await
    .expect_err("HTTP failure");
    let ToolError::Failed { source, .. } = error else {
        panic!("expected failure");
    };
    assert!(matches!(
        source.downcast_ref::<WebError>(),
        Some(WebError::Status { status: 401, .. })
    ));

    let malformed = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>bad</html>"))
        .mount(&malformed)
        .await;
    let error = run(
        WebSearchTool::with_config(config(&[(ENV_PROVIDER, "parallel")]))
            .with_endpoint(format!("{}/mcp", malformed.uri())),
        json!({ "queries": ["q"] }),
        context("ses_9"),
    )
    .await
    .expect_err("malformed response");
    let ToolError::Failed { source, .. } = error else {
        panic!("expected failure");
    };
    assert!(matches!(
        source.downcast_ref::<WebError>(),
        Some(WebError::MalformedSearchResponse {
            provider: "parallel"
        })
    ));
}

#[tokio::test]
async fn permission_is_checked_before_any_request() {
    let server = MockServer::start().await;
    mount_search(&server, "unreachable").await;
    let ctx = ToolContext::new(
        "ses_10",
        "msg_1",
        "call_1",
        "build",
        Arc::new(DenyAll),
        Arc::new(NeverInterrupted),
    );
    let error = run(
        WebSearchTool::with_config(config(&[(ENV_PROVIDER, "exa")]))
            .with_endpoint(format!("{}/mcp", server.uri())),
        json!({ "queries": ["one", "two"] }),
        ctx,
    )
    .await
    .expect_err("denied");
    assert!(matches!(error, ToolError::Denied { .. }));
    assert!(
        server
            .received_requests()
            .await
            .is_some_and(|requests| requests.is_empty())
    );
}

#[test]
fn provider_selection_is_explicit_and_deterministic() {
    let exa = WebSearchTool::with_config(config(&[(ENV_ENABLE_EXA, "true")]));
    assert_eq!(exa.provider_for("one"), Provider::Exa);
    assert_eq!(exa.provider_for("two"), Provider::Exa);

    let parallel = WebSearchTool::with_config(config(&[(ENV_ENABLE_PARALLEL, "true")]));
    assert_eq!(parallel.provider_for("one"), Provider::Parallel);
    assert_eq!(parallel.provider_for("two"), Provider::Parallel);
}
