//! End-to-end proof over a real child-process MCP server that validates its
//! arguments, which is the shape that failed in production: `chrome-devtools`
//! reported `Unknown argument for tool "list_pages": "intent"` and refused the
//! whole call, because the injected property was forwarded to a server whose
//! schema declares no arguments at all.
//!
//! Every layer here is the production one — `StdioClient` spawns and initializes a
//! real process, `Catalog`/`CatalogLoader` publish its tools, `ToolRegistryBuilder`
//! assembles them, and the call arrives through the `execute` composition. Only the
//! server is a stub, and only so its rejection is deterministic instead of
//! depending on which MCP servers a machine happens to have installed.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use zuno_config::schema::mcp::{LocalKind, McpLocal};
use zuno_mcp::{Catalog, PROTOCOL_VERSION, StdioClient, tool_name};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};
use zuno_tools::FileTools;
use zuno_tools::registry::{RegistryFlags, ToolRegistryBuilder};

const SERVER: &str = "chrome-devtools";
const TOOL: &str = "list_pages";

/// A server that declares no arguments and refuses any it is sent.
///
/// `case` on the exact `"arguments":{}` literal rather than parsing JSON: a shell
/// stub has no JSON parser, and an inexact match here would make the test pass for
/// the wrong reason.
fn write_strict_server(directory: &Path) -> PathBuf {
    let path = directory.join("strict-mcp-server.sh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"%s","capabilities":{},"serverInfo":{"name":"strict","version":"1"}}}\n' "$id" "$MCP_PROTOCOL_VERSION"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"list_pages","description":"Get a list of pages open in the browser.","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      case "$line" in
        *'"arguments":{}'*)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":false,"content":[{"type":"text","text":"1 page open"}]}}\n' "$id"
          ;;
        *)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":true,"content":[{"type":"text","text":"Unknown argument for tool \"list_pages\". This tool does not accept any arguments. Remove it and retry."}]}}\n' "$id"
          ;;
      esac
      ;;
  esac
done
"#,
    )
    .expect("write stub server");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make stub server executable");
    }
    path
}

fn context() -> ToolContext {
    ToolContext::new(
        "ses_strict",
        "msg_strict",
        "call_strict",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_schema_strict_stdio_server_is_called_through_batch_without_the_injected_keys() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let server_path = write_strict_server(workspace.path());

    let config = McpLocal {
        kind: LocalKind::Local,
        command: vec![
            "/bin/sh".to_owned(),
            server_path.to_string_lossy().into_owned(),
        ],
        cwd: None,
        environment: Some(BTreeMap::from([(
            "MCP_PROTOCOL_VERSION".to_owned(),
            PROTOCOL_VERSION.to_owned(),
        )])),
        enabled: None,
        timeout: NonZeroU32::new(10_000),
    };

    let client = Arc::new(
        StdioClient::connect(SERVER, workspace.path(), &config)
            .await
            .expect("the stub server completes the initialize handshake"),
    );
    let tools = client.list_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 1, "the stub publishes exactly one tool");

    let catalog = Catalog::new([SERVER]);
    catalog.connected(
        Arc::clone(&client) as Arc<dyn zuno_mcp::ConnectedServer>,
        tools,
    );

    let registry = ToolRegistryBuilder::new(
        workspace.path(),
        FileTools::new(workspace.path()).expect("file tools"),
        RegistryFlags {
            experimental_code_mode: true,
            ..RegistryFlags::default()
        },
    )
    .with_mcp_loader(Arc::new(catalog.loader()))
    .build();

    let namespaced = tool_name(SERVER, TOOL);
    assert!(
        registry.all().iter().any(|tool| tool.id() == namespaced),
        "the MCP tool must be registered as {namespaced}"
    );

    let output = registry
        .execute(
            "execute",
            json!({
                "tool_calls": [{
                    "tool": namespaced.clone(),
                    "intent": "check which pages are open"
                }]
            }),
            context(),
        )
        .await
        .expect("the batch completes");

    assert!(
        !output.output.contains("Unknown argument"),
        "the server rejected an argument the model never supplied: {}",
        output.output
    );
    assert!(
        output.output.contains("1 page open"),
        "the server's real answer must come back: {}",
        output.output
    );
    assert!(output.output.contains("Completed: 1 succeeded, 0 failed"));

    let direct = registry
        .execute(
            &namespaced,
            json!({ "intent": "the same call outside a batch" }),
            context(),
        )
        .await
        .expect("the direct registry path also succeeds");
    assert_eq!(direct.output, "1 page open");
}

/// Guards the premise: the stub really does refuse an undeclared argument.
///
/// Without this, a stub that silently accepted anything would make the test above
/// pass whether or not the strip happens.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_stub_server_refuses_an_injected_key_when_it_is_forwarded() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let server_path = write_strict_server(workspace.path());

    let config = McpLocal {
        kind: LocalKind::Local,
        command: vec![
            "/bin/sh".to_owned(),
            server_path.to_string_lossy().into_owned(),
        ],
        cwd: None,
        environment: Some(BTreeMap::from([(
            "MCP_PROTOCOL_VERSION".to_owned(),
            PROTOCOL_VERSION.to_owned(),
        )])),
        enabled: None,
        timeout: NonZeroU32::new(10_000),
    };

    let client = StdioClient::connect(SERVER, workspace.path(), &config)
        .await
        .expect("handshake");

    let leaked = client
        .call_tool(
            TOOL,
            serde_json::from_value::<serde_json::Map<String, Value>>(json!({ "intent": "leaked" }))
                .expect("arguments object"),
        )
        .await
        .expect("the server answers rather than dropping the call");

    assert!(
        leaked.is_error,
        "the stub must reject an undeclared argument, or it proves nothing"
    );
    assert!(
        format!("{:?}", leaked.content).contains("Unknown argument"),
        "{:?}",
        leaked.content
    );
}
