//! Catalog merge, resource tools, permission collapse, and rebuild-once.
//!
//! Every fake server here is in-memory. Todo 46's live test is the only network
//! target in this crate, and it stays that way: a merge rule must be provable
//! without a reachable server, or a CI outage becomes indistinguishable from a
//! regression.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use zuno_config::schema::permission::{PermissionAction, PermissionConfig};
use zuno_error::McpError;
use zuno_llm::cache::{LockedTools, McpToolStatus};
use zuno_mcp::catalog::{
    Catalog, CatalogEvent, ConnectedServer, LIST_RESOURCE_TEMPLATES_TOOL, LIST_RESOURCES_TOOL,
    PromptDefinition, READ_RESOURCE_TOOL, RESOURCE_TOOLS, ResourceContents, ResourceDefinition,
    ResourceTemplate, ServerStatus, resource_permission_patterns,
};
use zuno_permission::visibility::{READ_TOOLS, permission_key};
use zuno_permission::{Rule, rules_from_config};
use zuno_tool::{AllowAll, NeverInterrupted, Tool, ToolContext, ToolReplayPolicy};
use zuno_tools::registry::McpToolLoader;

struct FakeServer {
    name: String,
    tools: Vec<zuno_mcp::ToolDefinition>,
    resources: Vec<ResourceDefinition>,
    templates: Vec<ResourceTemplate>,
    prompts: Vec<PromptDefinition>,
    read: Option<ResourceContents>,
    supports_resources: bool,
    calls: AtomicUsize,
    last_call: std::sync::Mutex<Option<(String, Map<String, Value>)>>,
}

impl FakeServer {
    fn new(name: &str, tools: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            tools: tools.iter().map(|tool| definition(tool)).collect(),
            resources: Vec::new(),
            templates: Vec::new(),
            prompts: Vec::new(),
            read: None,
            supports_resources: false,
            calls: AtomicUsize::new(0),
            last_call: std::sync::Mutex::new(None),
        }
    }

    fn with_resources(mut self, resources: Vec<ResourceDefinition>) -> Self {
        self.supports_resources = true;
        self.resources = resources;
        self
    }

    fn with_templates(mut self, templates: Vec<ResourceTemplate>) -> Self {
        self.supports_resources = true;
        self.templates = templates;
        self
    }

    fn with_read(mut self, contents: ResourceContents) -> Self {
        self.supports_resources = true;
        self.read = Some(contents);
        self
    }

    fn with_prompts(mut self, prompts: Vec<PromptDefinition>) -> Self {
        self.prompts = prompts;
        self
    }

    fn tool_list(&self) -> Vec<zuno_mcp::ToolDefinition> {
        self.tools.clone()
    }
}

#[async_trait]
impl ConnectedServer for FakeServer {
    fn server_name(&self) -> &str {
        &self.name
    }

    fn supports_resources(&self) -> bool {
        self.supports_resources
    }

    fn supports_prompts(&self) -> bool {
        !self.prompts.is_empty()
    }

    async fn list_tools(&self) -> Result<Vec<zuno_mcp::ToolDefinition>, McpError> {
        Ok(self.tools.clone())
    }

    async fn call_tool(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<zuno_mcp::ToolCallResult, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_call.lock().expect("record call") = Some((tool.to_owned(), arguments));
        Ok(zuno_mcp::ToolCallResult {
            content: vec![json!({ "type": "text", "text": format!("{}:{tool}", self.name) })],
            structured_content: None,
            is_error: false,
            extra: Map::new(),
        })
    }

    async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
        Ok(self.resources.clone())
    }

    async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        Ok(self.templates.clone())
    }

    async fn read_resource(&self, uri: &str) -> Result<ResourceContents, McpError> {
        self.read.clone().ok_or_else(|| McpError::Connect {
            server: self.name.clone(),
            source: Box::new(std::io::Error::other(format!("no fixture for {uri}"))),
        })
    }

    async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
        Ok(self.prompts.clone())
    }
}

fn definition(name: &str) -> zuno_mcp::ToolDefinition {
    zuno_mcp::ToolDefinition {
        name: name.to_owned(),
        description: Some(format!("the {name} tool")),
        input_schema: json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
        }),
        output_schema: None,
        extra: Map::new(),
    }
}

fn resource(uri: &str, name: &str) -> ResourceDefinition {
    ResourceDefinition {
        uri: uri.to_owned(),
        name: name.to_owned(),
        description: None,
        mime_type: Some("text/plain".to_owned()),
        extra: Map::new(),
    }
}

fn template(uri_template: &str, name: &str) -> ResourceTemplate {
    ResourceTemplate {
        uri_template: uri_template.to_owned(),
        name: name.to_owned(),
        description: None,
        mime_type: None,
        extra: Map::new(),
    }
}

fn prompt(name: &str, arguments: &[&str]) -> PromptDefinition {
    PromptDefinition {
        name: name.to_owned(),
        description: Some(format!("{name} prompt")),
        arguments: arguments
            .iter()
            .map(|argument| zuno_mcp::catalog::PromptArgument {
                name: (*argument).to_owned(),
                description: None,
                required: false,
            })
            .collect(),
    }
}

/// Rules from a literal config object, so a test states `{"read": "deny"}` the
/// way a user writes it rather than hand-building `Rule` values.
fn rules(rules: Value) -> Vec<Rule> {
    let config: PermissionConfig = serde_json::from_value(json!({
        "mode": "standard",
        "rules": rules,
    }))
    .expect("permission config fixture parses");
    rules_from_config(&config)
}

fn connect(catalog: &Catalog, server: FakeServer) -> Arc<FakeServer> {
    let tools = server.tool_list();
    let prompts = server.prompts.clone();
    let handle = Arc::new(server);
    catalog.connected_with_prompts(
        Arc::clone(&handle) as Arc<dyn ConnectedServer>,
        tools,
        prompts,
    );
    handle
}

fn context() -> ToolContext {
    ToolContext::new(
        "ses_catalog",
        "msg_catalog",
        "call_catalog",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

fn ids(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools.iter().map(|tool| tool.id().to_owned()).collect()
}

#[test]
fn catalog_namespaces_a_tool_under_the_upstream_id() {
    let catalog = Catalog::new(["docs"]);
    connect(&catalog, FakeServer::new("docs", &["search"]));

    assert_eq!(
        catalog.tool_ids(),
        vec!["docs_search".to_owned()],
        "`search` on `docs` must be exposed as the oracle's sanitize(client)_sanitize(name)"
    );
    assert_eq!(
        catalog.tool_ids(),
        vec![zuno_mcp::tool_name("docs", "search")],
        "the id must come from the crate's one namespacing rule, not a second copy"
    );
}

#[test]
fn catalog_read_deny_removes_all_three_resource_tools_together() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_resources(vec![resource("mcp://a", "a")]),
    );

    let visible = ids(&catalog.tools());
    for tool in RESOURCE_TOOLS {
        assert!(
            visible.contains(&tool.to_owned()),
            "{tool} must be exposed before the deny: {visible:?}"
        );
    }

    let denied = ids(&catalog.visible_tools(&rules(json!({ "read": "deny" }))));
    for tool in RESOURCE_TOOLS {
        assert!(
            !denied.contains(&tool.to_owned()),
            "{{\"read\": \"deny\"}} must remove {tool}: {denied:?}"
        );
    }
    assert_eq!(
        denied,
        vec!["docs_search".to_owned()],
        "one `read` deny must remove exactly the three resource tools and nothing else"
    );
}

#[test]
fn catalog_resource_tool_names_are_the_names_permission_collapses() {
    assert_eq!(
        RESOURCE_TOOLS, READ_TOOLS,
        "renaming a resource tool silently breaks the `read` collapse in zuno-permission"
    );
    for tool in RESOURCE_TOOLS {
        assert_eq!(
            permission_key(tool),
            "read",
            "{tool} must be governed by the shared `read` key, not by its own name"
        );
    }
}

#[test]
fn catalog_two_servers_sharing_a_tool_name_do_not_collide() {
    let catalog = Catalog::new(["docs", "wiki"]);
    connect(&catalog, FakeServer::new("docs", &["search"]));
    connect(&catalog, FakeServer::new("wiki", &["search"]));

    assert_eq!(
        catalog.tool_ids(),
        vec!["docs_search".to_owned(), "wiki_search".to_owned()],
        "identically named tools must survive the merge under distinct namespaced ids"
    );
}

#[test]
fn catalog_loader_marks_only_explicit_session_servers_eager() {
    let catalog = Catalog::new_with_eager_servers(["global", "session"], ["session"]);
    connect(&catalog, FakeServer::new("global", &["search"]));
    connect(
        &catalog,
        FakeServer::new("session", &["lookup"])
            .with_resources(vec![resource("mcp://session", "session")]),
    );

    assert_eq!(
        catalog.loader().eager_tool_ids(),
        [
            vec!["session_lookup".to_owned()],
            RESOURCE_TOOLS.map(str::to_owned).to_vec(),
        ]
        .concat()
    );
    assert_eq!(
        catalog.tool_ids(),
        [
            vec!["global_search".to_owned(), "session_lookup".to_owned()],
            RESOURCE_TOOLS.map(str::to_owned).to_vec(),
        ]
        .concat(),
        "the eager marker must not remove or reorder the merged catalog"
    );
}

#[tokio::test]
async fn catalog_namespaced_call_reaches_the_right_server_under_the_local_name() {
    let catalog = Catalog::new(["docs", "wiki"]);
    let docs = connect(&catalog, FakeServer::new("docs", &["search"]));
    let wiki = connect(&catalog, FakeServer::new("wiki", &["search"]));

    let tools = catalog.tools();
    let wiki_tool = tools
        .iter()
        .find(|tool| tool.id() == "wiki_search")
        .expect("wiki_search is exposed");
    let output = wiki_tool
        .execute(json!({ "query": "rust" }), context())
        .await
        .expect("call succeeds");

    assert_eq!(output.output, "wiki:search");
    assert_eq!(
        docs.calls.load(Ordering::SeqCst),
        0,
        "docs must not be called"
    );
    assert_eq!(wiki.calls.load(Ordering::SeqCst), 1);
    let (called, arguments) = wiki
        .last_call
        .lock()
        .expect("read recorded call")
        .clone()
        .expect("a call was recorded");
    assert_eq!(
        called, "search",
        "the server must be addressed by its own tool name, never the namespaced id"
    );
    assert_eq!(arguments.get("query").and_then(Value::as_str), Some("rust"));
}

#[test]
fn catalog_failed_server_contributes_zero_tools_and_a_named_diagnostic() {
    let catalog = Catalog::new(["docs", "broken"]);
    connect(&catalog, FakeServer::new("docs", &["search"]));
    let broken = Arc::new(FakeServer::new("broken", &["dangerous"]));
    catalog.connected(
        Arc::clone(&broken) as Arc<dyn ConnectedServer>,
        broken.tool_list(),
    );
    catalog.unavailable(
        "broken",
        ServerStatus::Failed {
            error: "Connection closed".to_owned(),
        },
    );

    assert_eq!(
        catalog.tool_ids(),
        vec!["docs_search".to_owned()],
        "a server that failed must contribute zero tools even though its snapshot is retained"
    );
    assert_eq!(catalog.connected_servers(), vec!["docs".to_owned()]);

    let diagnostics = catalog.diagnostics();
    assert_eq!(diagnostics.len(), 1, "exactly one server is unavailable");
    assert_eq!(diagnostics[0].server, "broken");
    assert_eq!(
        diagnostics[0].message(),
        "MCP server broken is unavailable and contributes no tools: Connection closed",
        "the diagnostic must name the server that is missing"
    );
}

#[test]
fn catalog_never_connected_server_contributes_zero_tools_and_names_itself() {
    let catalog = Catalog::new(["docs", "absent"]);
    connect(&catalog, FakeServer::new("docs", &["search"]));
    catalog.unavailable(
        "absent",
        ServerStatus::Failed {
            error: "could not spawn absent-server".to_owned(),
        },
    );

    assert_eq!(catalog.tool_ids(), vec!["docs_search".to_owned()]);
    let messages: Vec<String> = catalog
        .diagnostics()
        .iter()
        .map(zuno_mcp::Diagnostic::message)
        .collect();
    assert_eq!(
        messages,
        vec![
            "MCP server absent is unavailable and contributes no tools: could not spawn absent-server"
                .to_owned()
        ]
    );
}

#[test]
fn catalog_diagnostics_distinguish_every_unavailable_reason() {
    let catalog = Catalog::new(["off", "auth", "registration"]);
    catalog.unavailable("off", ServerStatus::Disabled);
    catalog.unavailable("auth", ServerStatus::NeedsAuth);
    catalog.unavailable(
        "registration",
        ServerStatus::NeedsClientRegistration {
            error: "no clientId".to_owned(),
        },
    );

    let labelled: Vec<(String, &str)> = catalog
        .diagnostics()
        .into_iter()
        .map(|diagnostic| (diagnostic.server, diagnostic.status.label()))
        .collect();
    assert_eq!(
        labelled,
        vec![
            ("auth".to_owned(), "needs_auth"),
            ("off".to_owned(), "disabled"),
            ("registration".to_owned(), "needs_client_registration"),
        ]
    );
    assert!(catalog.tool_ids().is_empty());
    let auth = catalog
        .diagnostics()
        .into_iter()
        .find(|diagnostic| diagnostic.server == "auth")
        .expect("auth diagnostic");
    assert!(
        auth.message().contains("zuno mcp auth auth"),
        "the needs_auth diagnostic must say how to fix it: {}",
        auth.message()
    );
}

#[test]
fn catalog_resource_tools_absent_until_a_connected_server_serves_resources() {
    let catalog = Catalog::new(["docs"]);
    connect(&catalog, FakeServer::new("docs", &["search"]));

    assert_eq!(
        catalog.tool_ids(),
        vec!["docs_search".to_owned()],
        "no connected server declares resources, so the three resource tools stay unregistered"
    );
    assert!(catalog.resource_servers().is_empty());

    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_resources(vec![resource("mcp://a", "a")]),
    );
    assert_eq!(
        catalog.tool_ids(),
        vec![
            "docs_search".to_owned(),
            LIST_RESOURCES_TOOL.to_owned(),
            LIST_RESOURCE_TEMPLATES_TOOL.to_owned(),
            READ_RESOURCE_TOOL.to_owned(),
        ]
    );
}

#[test]
fn catalog_resource_tools_withdrawn_when_the_resource_server_fails() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_resources(vec![resource("mcp://a", "a")]),
    );
    catalog.unavailable(
        "docs",
        ServerStatus::Failed {
            error: "Connection closed".to_owned(),
        },
    );

    assert!(
        catalog.tool_ids().is_empty(),
        "the resource tools have nothing to read once no connected server serves resources"
    );
}

#[tokio::test]
async fn catalog_list_resources_merges_servers_and_relabels_client_as_server() {
    let catalog = Catalog::new(["wiki", "docs"]);
    connect(
        &catalog,
        FakeServer::new("wiki", &[]).with_resources(vec![resource("mcp://w/1", "w-one")]),
    );
    connect(
        &catalog,
        FakeServer::new("docs", &[]).with_resources(vec![
            resource("mcp://d/2", "d-two"),
            resource("mcp://d/1", "d-one"),
        ]),
    );

    let tools = catalog.tools();
    let list = tools
        .iter()
        .find(|tool| tool.id() == LIST_RESOURCES_TOOL)
        .expect("list_mcp_resources is exposed");
    let output = list
        .execute(json!({}), context())
        .await
        .expect("listing succeeds");
    let payload: Value = serde_json::from_str(&output.output).expect("output is JSON");
    let listed = payload["resources"].as_array().expect("resources array");

    assert_eq!(listed.len(), 3);
    let ordered: Vec<(&str, &str)> = listed
        .iter()
        .map(|item| {
            (
                item["server"].as_str().expect("server label"),
                item["uri"].as_str().expect("uri"),
            )
        })
        .collect();
    assert_eq!(
        ordered,
        vec![
            ("docs", "mcp://d/1"),
            ("docs", "mcp://d/2"),
            ("wiki", "mcp://w/1"),
        ],
        "the oracle sorts by client, then name, then uri"
    );
    for item in listed {
        assert!(
            item.get("client").is_none(),
            "the internal `client` field must be rewritten as `server`"
        );
    }
    assert_eq!(output.metadata["count"], json!(3));
    assert_eq!(output.metadata["servers"], json!(["docs", "wiki"]));
}

#[tokio::test]
async fn catalog_list_resources_refuses_an_unknown_server_and_names_the_real_ones() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &[]).with_resources(vec![resource("mcp://a", "a")]),
    );

    let tools = catalog.tools();
    let list = tools
        .iter()
        .find(|tool| tool.id() == LIST_RESOURCES_TOOL)
        .expect("list_mcp_resources is exposed");
    let error = list
        .execute(json!({ "server": "invented" }), context())
        .await
        .expect_err("an unknown server is refused");
    let rendered = std::error::Error::source(&error)
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        rendered.contains("does not support resources") && rendered.contains("docs"),
        "the refusal must list the servers that do exist: {rendered}"
    );
}

#[tokio::test]
async fn catalog_list_resource_templates_reports_the_template_field() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &[]).with_templates(vec![template("mcp://d/{id}", "by-id")]),
    );

    let tools = catalog.tools();
    let list = tools
        .iter()
        .find(|tool| tool.id() == LIST_RESOURCE_TEMPLATES_TOOL)
        .expect("list_mcp_resource_templates is exposed");
    let output = list
        .execute(json!({ "server": "docs" }), context())
        .await
        .expect("listing succeeds");
    let payload: Value = serde_json::from_str(&output.output).expect("output is JSON");
    assert_eq!(
        payload["resourceTemplates"][0]["uriTemplate"],
        json!("mcp://d/{id}")
    );
    assert_eq!(payload["resourceTemplates"][0]["server"], json!("docs"));
    assert_eq!(output.title, "MCP resource templates: docs");
}

#[tokio::test]
async fn catalog_read_resource_renders_text_and_requires_both_arguments() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &[]).with_read(ResourceContents {
            contents: vec![json!({
                "uri": "mcp://d/1",
                "mimeType": "text/markdown",
                "text": "# hello",
            })],
        }),
    );

    let tools = catalog.tools();
    let read = tools
        .iter()
        .find(|tool| tool.id() == READ_RESOURCE_TOOL)
        .expect("read_mcp_resource is exposed");
    let output = read
        .execute(json!({ "server": "docs", "uri": "mcp://d/1" }), context())
        .await
        .expect("read succeeds");
    assert_eq!(
        output.output,
        "Resource: mcp://d/1\nMIME: text/markdown\n# hello"
    );
    assert_eq!(output.metadata["contents"], json!(1));
    assert_eq!(output.metadata["attachments"], json!(0));

    let error = read
        .execute(json!({ "server": "docs" }), context())
        .await
        .expect_err("a missing uri is refused");
    assert_eq!(
        std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default(),
        "uri is required"
    );
}

#[tokio::test]
async fn catalog_read_resource_attaches_a_supported_blob_and_omits_the_rest() {
    let oversized = "A".repeat(4 * (10 * 1024 * 1024 / 3 + 32));
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &[]).with_read(ResourceContents {
            contents: vec![
                json!({ "uri": "mcp://png", "mimeType": "image/png", "blob": "aGk=" }),
                json!({ "uri": "mcp://bin", "mimeType": "application/zip", "blob": "aGk=" }),
                json!({ "uri": "mcp://big", "mimeType": "image/png", "blob": oversized }),
                json!({ "uri": "mcp://empty" }),
            ],
        }),
    );

    let tools = catalog.tools();
    let read = tools
        .iter()
        .find(|tool| tool.id() == READ_RESOURCE_TOOL)
        .expect("read_mcp_resource is exposed");
    let output = read
        .execute(json!({ "server": "docs", "uri": "mcp://png" }), context())
        .await
        .expect("read succeeds");

    assert_eq!(output.attachments.len(), 1, "only the small PNG attaches");
    assert_eq!(output.attachments[0].mime, "image/png");
    assert_eq!(output.attachments[0].url, "data:image/png;base64,aGk=");
    assert_eq!(
        output.attachments[0].filename.as_deref(),
        Some("mcp://png"),
        "the attachment keeps the resource uri as its filename"
    );
    assert!(
        output
            .output
            .contains("[Binary MCP resource attached: mcp://png (image/png)]")
    );
    assert!(
        output
            .output
            .contains("is not a supported attachment type]"),
        "an unsupported mime is described rather than attached: {}",
        output.output
    );
    assert!(
        output.output.contains("exceeds 10 MB]"),
        "an oversized blob is described rather than attached: {}",
        output.output
    );
    assert!(
        output
            .output
            .contains("[MCP resource content without text or blob: mcp://empty]")
    );
}

#[tokio::test]
async fn catalog_read_resource_reports_an_empty_payload_rather_than_empty_text() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &[]).with_read(ResourceContents::default()),
    );

    let tools = catalog.tools();
    let read = tools
        .iter()
        .find(|tool| tool.id() == READ_RESOURCE_TOOL)
        .expect("read_mcp_resource is exposed");
    let output = read
        .execute(json!({ "server": "docs", "uri": "mcp://none" }), context())
        .await
        .expect("read succeeds");
    assert_eq!(
        output.output,
        "MCP resource mcp://none from docs returned no contents."
    );
}

#[test]
fn catalog_resource_permission_patterns_narrow_to_the_addressed_server() {
    let servers = vec!["docs".to_owned(), "wiki".to_owned()];
    assert_eq!(
        resource_permission_patterns(Some("docs"), &servers),
        vec!["mcp:docs:*".to_owned()]
    );
    assert_eq!(
        resource_permission_patterns(None, &servers),
        vec!["mcp:docs:*".to_owned(), "mcp:wiki:*".to_owned()]
    );
}

#[tokio::test]
async fn catalog_refresh_republishes_the_changed_server_and_replaces_its_tools() {
    let catalog = Catalog::new(["docs"]);
    let mut events = catalog.subscribe();
    let handle = connect(&catalog, FakeServer::new("docs", &["search"]));

    assert_eq!(
        events.recv().await.expect("connection publishes an event"),
        CatalogEvent::ToolsChanged {
            server: "docs".to_owned()
        }
    );

    let replacement = Arc::new(FakeServer::new("docs", &["search", "lookup"]));
    catalog.connected(
        Arc::clone(&replacement) as Arc<dyn ConnectedServer>,
        replacement.tool_list(),
    );
    assert_eq!(
        events.recv().await.expect("re-registration publishes"),
        CatalogEvent::ToolsChanged {
            server: "docs".to_owned()
        }
    );
    assert_eq!(
        catalog.tool_ids(),
        vec!["docs_search".to_owned(), "docs_lookup".to_owned()],
        "servers are name-ordered, but one server's tools keep the order it advertised"
    );

    let refreshed = catalog.refresh("docs").await.expect("refresh succeeds");
    assert_eq!(refreshed.len(), 2);
    assert_eq!(
        events.recv().await.expect("refresh publishes"),
        CatalogEvent::ToolsChanged {
            server: "docs".to_owned()
        }
    );
    assert_eq!(handle.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn catalog_refresh_refuses_a_server_that_is_not_connected() {
    let catalog = Catalog::new(["docs"]);
    catalog.unavailable(
        "docs",
        ServerStatus::Failed {
            error: "Connection closed".to_owned(),
        },
    );

    let error = catalog
        .refresh("docs")
        .await
        .expect_err("a failed server cannot be refreshed");
    assert_eq!(error.to_string(), "mcp server docs is not connected");
    let error = catalog
        .refresh("never-configured")
        .await
        .expect_err("an unknown server cannot be refreshed");
    assert_eq!(
        error.to_string(),
        "mcp server never-configured is not connected"
    );
}

#[test]
fn catalog_discovery_stays_pending_until_every_expected_server_reports() {
    let catalog = Catalog::new(["docs", "broken"]);
    assert_eq!(catalog.discovery_status(), McpToolStatus::Pending);

    connect(&catalog, FakeServer::new("docs", &["search"]));
    assert_eq!(
        catalog.discovery_status(),
        McpToolStatus::Pending,
        "one server still owes a report"
    );

    catalog.unavailable(
        "broken",
        ServerStatus::Failed {
            error: "Connection closed".to_owned(),
        },
    );
    assert_eq!(
        catalog.discovery_status(),
        McpToolStatus::Ready,
        "a failure is a report; discovery has settled"
    );

    assert_eq!(
        Catalog::new(Vec::<String>::new()).discovery_status(),
        McpToolStatus::Ready,
        "no configured servers means nothing to wait for"
    );
}

#[test]
fn catalog_late_connection_rebuilds_the_locked_tool_list_exactly_once() {
    let catalog = Catalog::new(["docs", "wiki"]);
    let mut locked = LockedTools::new();

    let first = catalog.tools_for_request(&mut locked);
    assert!(first.tools().is_empty());
    assert!(!first.rebuilt_for_late_mcp());
    assert_eq!(locked.rebuild_count(), 0);

    connect(&catalog, FakeServer::new("docs", &["search"]));
    let pending = catalog.tools_for_request(&mut locked);
    assert!(
        pending.tools().is_empty(),
        "while discovery is pending the frozen list must not move"
    );
    assert!(!pending.rebuilt_for_late_mcp());
    assert_eq!(locked.rebuild_count(), 0);

    connect(&catalog, FakeServer::new("wiki", &["search"]));
    let rebuilt = catalog.tools_for_request(&mut locked);
    assert!(
        rebuilt.rebuilt_for_late_mcp(),
        "discovery settled with a changed list, so the one rebuild is spent"
    );
    assert_eq!(
        rebuilt.tools(),
        ["docs_search".to_owned(), "wiki_search".to_owned()]
    );
    assert_eq!(locked.rebuild_count(), 1);

    let handle = Arc::new(FakeServer::new("late", &["search"]));
    catalog.connected(
        Arc::clone(&handle) as Arc<dyn ConnectedServer>,
        handle.tool_list(),
    );
    let after = catalog.tools_for_request(&mut locked);
    assert!(
        !after.rebuilt_for_late_mcp(),
        "a second late connection must NOT rebuild the list again"
    );
    assert_eq!(
        after.tools(),
        ["docs_search".to_owned(), "wiki_search".to_owned()],
        "the frozen list must still exclude the late server's tools"
    );
    assert_eq!(
        locked.rebuild_count(),
        1,
        "the rebuild allowance is one, not one per late connection"
    );

    for _ in 0..5 {
        let repeat = catalog.tools_for_request(&mut locked);
        assert!(!repeat.rebuilt_for_late_mcp());
        assert_eq!(locked.rebuild_count(), 1);
    }
}

#[test]
fn catalog_settled_first_request_never_spends_the_rebuild() {
    let catalog = Catalog::new(["docs"]);
    connect(&catalog, FakeServer::new("docs", &["search"]));
    let mut locked = LockedTools::new();

    let first = catalog.tools_for_request(&mut locked);
    assert_eq!(first.tools(), ["docs_search".to_owned()]);
    assert!(!first.rebuilt_for_late_mcp());

    let handle = Arc::new(FakeServer::new("late", &["search"]));
    catalog.connected(
        Arc::clone(&handle) as Arc<dyn ConnectedServer>,
        handle.tool_list(),
    );
    let second = catalog.tools_for_request(&mut locked);
    assert!(
        !second.rebuilt_for_late_mcp(),
        "discovery was already settled on the first request, so nothing may rebuild"
    );
    assert_eq!(second.tools(), ["docs_search".to_owned()]);
    assert_eq!(locked.rebuild_count(), 0);
}

#[test]
fn catalog_prompts_come_only_from_connected_servers() {
    let catalog = Catalog::new(["docs", "broken"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_prompts(vec![prompt("explain", &["topic"])]),
    );
    let broken = Arc::new(FakeServer::new("broken", &[]).with_prompts(vec![prompt("ghost", &[])]));
    catalog.connected_with_prompts(
        Arc::clone(&broken) as Arc<dyn ConnectedServer>,
        Vec::new(),
        broken.prompts.clone(),
    );
    catalog.unavailable(
        "broken",
        ServerStatus::Failed {
            error: "Connection closed".to_owned(),
        },
    );

    let prompts = catalog.prompts();
    assert_eq!(prompts.len(), 1, "a failed server contributes no prompts");
    assert_eq!(prompts[0].client, "docs");
    assert_eq!(prompts[0].prompt, "explain");
    assert_eq!(prompts[0].arguments, vec!["topic".to_owned()]);
    assert_eq!(
        prompts[0].command_name(),
        "docs:explain",
        "wave 3 addresses an MCP prompt as sanitize(client):sanitize(prompt)"
    );
}

#[test]
fn catalog_prompts_reach_the_command_resolver_at_level_three() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_prompts(vec![prompt("explain", &["topic"])]),
    );

    let prompts = catalog.prompts();
    let sources = zuno_catalog::command::Sources::new("/workspace").with_mcp_prompts(&prompts);
    let registry = zuno_catalog::command::Registry::build(&sources);

    assert!(
        registry.get("docs:explain").is_some(),
        "the catalog's prompts must be resolvable as commands: {:?}",
        registry.names().collect::<Vec<_>>()
    );
}

#[test]
fn catalog_loader_supplies_the_registry_seam_unfiltered() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_resources(vec![resource("mcp://a", "a")]),
    );

    let loader = catalog.loader();
    assert_eq!(
        ids(&loader.tools()),
        catalog.tool_ids(),
        "the loader hands the registry the whole merged list; the registry hides"
    );
}

#[test]
fn catalog_tool_definitions_carry_the_server_schema_forced_into_object_shape() {
    let catalog = Catalog::new(["docs"]);
    let mut schemaless = definition("search");
    schemaless.input_schema = json!({ "description": "no type, no properties" });
    let handle = Arc::new(FakeServer {
        tools: vec![schemaless],
        ..FakeServer::new("docs", &[])
    });
    catalog.connected(
        Arc::clone(&handle) as Arc<dyn ConnectedServer>,
        handle.tool_list(),
    );

    let tools = catalog.tools();
    let raw = tools[0].raw_parameters_schema();
    assert_eq!(raw["type"], json!("object"));
    assert_eq!(raw["properties"], json!({}));
    assert_eq!(raw["additionalProperties"], json!(false));
    assert_eq!(
        raw["description"], "no type, no properties",
        "the server's own schema fields survive the coercion"
    );

    let definition = tools[0].definition();
    assert_eq!(definition.id, "docs_search");
    assert_eq!(
        definition.parameters["properties"][zuno_tool::INTENT_KEY]["type"],
        json!("string"),
        "an MCP proxy still passes through the central schema augmentation"
    );
}

#[tokio::test]
async fn catalog_tool_level_error_becomes_a_failure_naming_the_namespaced_tool() {
    struct Failing;

    #[async_trait]
    impl ConnectedServer for Failing {
        fn server_name(&self) -> &str {
            "docs"
        }
        fn supports_resources(&self) -> bool {
            false
        }
        fn supports_prompts(&self) -> bool {
            false
        }
        async fn list_tools(&self) -> Result<Vec<zuno_mcp::ToolDefinition>, McpError> {
            Ok(vec![definition("search")])
        }
        async fn call_tool(
            &self,
            _tool: &str,
            _arguments: Map<String, Value>,
        ) -> Result<zuno_mcp::ToolCallResult, McpError> {
            Ok(zuno_mcp::ToolCallResult {
                content: vec![json!({ "type": "text", "text": "index is offline" })],
                structured_content: None,
                is_error: true,
                extra: Map::new(),
            })
        }
        async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
            Ok(Vec::new())
        }
        async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
            Ok(Vec::new())
        }
        async fn read_resource(&self, _uri: &str) -> Result<ResourceContents, McpError> {
            Ok(ResourceContents::default())
        }
        async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
            Ok(Vec::new())
        }
    }

    let catalog = Catalog::new(["docs"]);
    catalog.connected(Arc::new(Failing), vec![definition("search")]);

    let tools = catalog.tools();
    let error = tools[0]
        .execute(json!({}), context())
        .await
        .expect_err("a tool-level error is a failure, not a success");
    assert_eq!(error.tool(), "docs_search");
    assert_eq!(
        std::error::Error::source(&error)
            .map(ToString::to_string)
            .unwrap_or_default(),
        "index is offline",
        "the server's own message must survive so a user can act on it"
    );
}

#[tokio::test]
async fn catalog_non_object_arguments_are_refused_before_the_transport() {
    let catalog = Catalog::new(["docs"]);
    let handle = connect(&catalog, FakeServer::new("docs", &["search"]));

    let tools = catalog.tools();
    let error = tools[0]
        .execute(json!("not an object"), context())
        .await
        .expect_err("a non-object argument value is invalid");
    assert_eq!(error.tool(), "docs_search");
    assert!(error.is_model_correctable());
    assert_eq!(
        handle.calls.load(Ordering::SeqCst),
        0,
        "invalid arguments must not reach the server"
    );

    let output = tools[0]
        .execute(Value::Null, context())
        .await
        .expect("a null argument value means no arguments");
    assert_eq!(output.output, "docs:search");
    assert_eq!(handle.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn catalog_namespacing_sanitizes_both_halves_of_the_id() {
    let catalog = Catalog::new(["my docs"]);
    connect(&catalog, FakeServer::new("my docs", &["search/all"]));

    assert_eq!(
        catalog.tool_ids(),
        vec!["my_docs_search_all".to_owned()],
        "characters outside [a-zA-Z0-9_-] become underscores in both halves"
    );
}

#[test]
fn catalog_wildcard_deny_hides_the_namespaced_tools_too() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_resources(vec![resource("mcp://a", "a")]),
    );

    assert!(
        catalog
            .visible_tools(&rules(json!({ "*": "deny" })))
            .is_empty(),
        "an outer wildcard deny hides every merged tool, resource tools included"
    );
    assert_eq!(
        ids(&catalog.visible_tools(&rules(json!({ "docs_search": "deny" })))),
        RESOURCE_TOOLS.map(str::to_owned).to_vec(),
        "a namespaced tool is denied by its own id, leaving the resource tools alone"
    );
}

#[test]
fn catalog_narrow_read_deny_leaves_the_resource_tools_visible() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_resources(vec![resource("mcp://a", "a")]),
    );

    let narrow = rules(json!({ "read": { "mcp:docs:*": "deny" } }));
    assert!(
        narrow
            .iter()
            .all(|rule| rule.action == PermissionAction::Deny),
        "fixture sanity: the narrow rule really is a deny"
    );
    let visible = ids(&catalog.visible_tools(&narrow));
    for tool in RESOURCE_TOOLS {
        assert!(
            visible.contains(&tool.to_owned()),
            "a pattern-scoped deny is enforced at call time, not by hiding {tool}: {visible:?}"
        );
    }
}

/// A server whose every method fails with one caller-chosen `McpError`.
///
/// Separate from [`FakeServer`], which exists to prove merge and rendering rules on a
/// server that works. This one exists to prove that the *class* of a transport failure
/// survives the trip to the tool layer.
struct BrokenServer {
    error: Box<dyn Fn() -> McpError + Send + Sync>,
}

impl BrokenServer {
    fn new(error: impl Fn() -> McpError + Send + Sync + 'static) -> Self {
        Self {
            error: Box::new(error),
        }
    }

    fn fail<T>(&self) -> Result<T, McpError> {
        Err((self.error)())
    }
}

#[async_trait]
impl ConnectedServer for BrokenServer {
    fn server_name(&self) -> &str {
        "docs"
    }

    fn supports_resources(&self) -> bool {
        true
    }

    fn supports_prompts(&self) -> bool {
        false
    }

    async fn list_tools(&self) -> Result<Vec<zuno_mcp::ToolDefinition>, McpError> {
        self.fail()
    }

    async fn call_tool(
        &self,
        _tool: &str,
        _arguments: Map<String, Value>,
    ) -> Result<zuno_mcp::ToolCallResult, McpError> {
        self.fail()
    }

    async fn list_resources(&self) -> Result<Vec<ResourceDefinition>, McpError> {
        self.fail()
    }

    async fn list_resource_templates(&self) -> Result<Vec<ResourceTemplate>, McpError> {
        self.fail()
    }

    async fn read_resource(&self, _uri: &str) -> Result<ResourceContents, McpError> {
        self.fail()
    }

    async fn list_prompts(&self) -> Result<Vec<PromptDefinition>, McpError> {
        Ok(Vec::new())
    }
}

fn broken_catalog(error: impl Fn() -> McpError + Send + Sync + 'static) -> Catalog {
    let catalog = Catalog::new(["docs"]);
    catalog.connected(
        Arc::new(BrokenServer::new(error)) as Arc<dyn ConnectedServer>,
        vec![definition("search")],
    );
    catalog
}

fn tool_named(catalog: &Catalog, id: &str) -> Arc<dyn Tool> {
    catalog
        .tools()
        .into_iter()
        .find(|tool| tool.id() == id)
        .unwrap_or_else(|| panic!("{id} must be exposed"))
}

#[tokio::test]
async fn catalog_relays_an_mcp_timeout_as_a_retryable_tool_timeout() {
    let elapsed = std::time::Duration::from_secs(11);
    let catalog = broken_catalog(move || McpError::Timeout {
        server: "docs".to_owned(),
        elapsed,
    });

    for id in ["docs_search", LIST_RESOURCES_TOOL, READ_RESOURCE_TOOL] {
        let args = if id == READ_RESOURCE_TOOL {
            json!({ "server": "docs", "uri": "mcp://a" })
        } else {
            json!({})
        };
        let error = tool_named(&catalog, id)
            .execute(args, context())
            .await
            .expect_err("a timed-out server is a tool failure");
        assert_eq!(error.tool(), id);
        assert!(
            matches!(&error, zuno_error::ToolError::Timeout { elapsed: seen, .. } if *seen == elapsed),
            "{id} must preserve the elapsed time instead of flattening it into a failure: {error:?}"
        );
        assert!(
            error.recovery().is_retry(),
            "{id} timed out, which the engine must be able to schedule again"
        );
    }
}

#[tokio::test]
async fn catalog_relays_a_connect_failure_as_a_retryable_transient() {
    let catalog = broken_catalog(|| McpError::Connect {
        server: "docs".to_owned(),
        source: Box::new(std::io::Error::other("connection refused")),
    });

    for id in ["docs_search", LIST_RESOURCE_TEMPLATES_TOOL] {
        let error = tool_named(&catalog, id)
            .execute(json!({}), context())
            .await
            .expect_err("an unreachable server is a tool failure");
        assert!(
            matches!(error, zuno_error::ToolError::Transient { .. }),
            "{id} must report a server that is still coming up as transient: {error:?}"
        );
        assert!(error.recovery().is_retry());
    }
}

#[tokio::test]
async fn catalog_keeps_protocol_and_handshake_failures_permanent() {
    let permanent: [Box<dyn Fn() -> McpError + Send + Sync>; 3] = [
        Box::new(|| McpError::Protocol {
            server: "docs".to_owned(),
            source: serde_json::from_str::<Value>("{\"jsonrpc\":").unwrap_err(),
        }),
        Box::new(|| McpError::Handshake {
            server: "docs".to_owned(),
            source: Box::new(std::io::Error::other("unsupported protocol version")),
        }),
        Box::new(|| McpError::ToolCall {
            server: "docs".to_owned(),
            tool: "search".to_owned(),
            source: Box::new(std::io::Error::other("target closed")),
        }),
    ];

    for case in permanent {
        let catalog = broken_catalog(case);
        let error = tool_named(&catalog, "docs_search")
            .execute(json!({}), context())
            .await
            .expect_err("a protocol or handshake failure is a tool failure");
        assert!(
            matches!(error, zuno_error::ToolError::Failed { .. }),
            "an invalid exchange must block rather than be retried: {error:?}"
        );
        assert!(!error.recovery().is_retry());
    }
}

#[test]
fn catalog_relayed_server_tools_are_never_replayable_and_reads_are_safe() {
    let catalog = Catalog::new(["docs"]);
    connect(
        &catalog,
        FakeServer::new("docs", &["search"]).with_resources(vec![resource("mcp://a", "a")]),
    );

    let proxy = tool_named(&catalog, "docs_search");
    assert_eq!(
        proxy.replay_policy(),
        ToolReplayPolicy::Never,
        "a relayed server tool may create, send, or write; a lost response does not \
         prove it did not happen"
    );
    assert_eq!(
        proxy.replay_policy_for(&json!({ "query": "anything" })),
        ToolReplayPolicy::Never,
        "no argument shape makes an arbitrary upstream tool idempotent"
    );

    for id in RESOURCE_TOOLS {
        let tool = tool_named(&catalog, id);
        assert_eq!(
            tool.replay_policy(),
            ToolReplayPolicy::Safe,
            "{id} only issues MCP reads, so an identical retry is safe"
        );
        assert_eq!(
            tool.effect(&json!({})),
            zuno_tool::ToolEffect::ReadOnly,
            "{id} declares itself read-only, which is what makes the replay policy honest"
        );
    }
}
