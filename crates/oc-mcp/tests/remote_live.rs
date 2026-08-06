use std::time::Duration;

use oc_config::schema::mcp::McpRemote;
use oc_mcp::{RemoteClient, RemoteConnect};
use serde_json::{Value, json};

const CONFIG: &str = "/config/.config/opencode/opencode.json";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_live_configured_servers_handshake_and_list_real_tools() {
    let text = match tokio::fs::read_to_string(CONFIG).await {
        Ok(text) => text,
        Err(error) => {
            eprintln!("SKIP remote live MCP test: cannot read {CONFIG}: {error}");
            return;
        }
    };
    let document: Value = match serde_json::from_str(&strip_jsonc(&text)) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("SKIP remote live MCP test: cannot parse {CONFIG}: {error}");
            return;
        }
    };

    let mut reached = 0_usize;
    for name in ["aws-knowledge-mcp-server", "microsoft-learn"] {
        let Some(value) = document.pointer(&format!("/mcp/{name}")) else {
            eprintln!("SKIP remote live MCP endpoint {name}: not configured in {CONFIG}");
            continue;
        };
        let config: McpRemote =
            serde_json::from_value(value.clone()).expect("configured remote shape");
        if !reachable(&config.url).await {
            eprintln!(
                "SKIP remote live MCP endpoint {name}: {} is unreachable",
                config.url
            );
            continue;
        }
        reached += 1;

        let outcome = RemoteClient::connect(name, &config)
            .await
            .unwrap_or_else(|error| panic!("reachable remote {name} failed handshake: {error}"));
        let RemoteConnect::Connected(client) = outcome else {
            panic!("configured no-auth endpoint {name} unexpectedly requested OAuth")
        };
        let tools = client
            .list_tools()
            .await
            .unwrap_or_else(|error| panic!("reachable remote {name} failed tools/list: {error}"));
        assert!(
            !tools.is_empty(),
            "reachable remote {name} returned an empty tool list"
        );

        eprintln!(
            "LIVE REMOTE MCP {name} initialize: {}",
            json!({ "jsonrpc": "2.0", "id": 1, "result": client.initialization() })
        );
        eprintln!(
            "LIVE REMOTE MCP {name} tools/list: {}",
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": tools } })
        );
        client.close().await;
    }

    if reached == 0 {
        eprintln!("SKIP remote live MCP test: neither configured public endpoint was reachable");
    }
}

async fn reachable(url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    client.get(url).send().await.is_ok()
}

fn strip_jsonc(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        String,
        LineComment,
        BlockComment,
    }

    let mut bytes = text.as_bytes().to_vec();
    let mut state = State::Normal;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        match state {
            State::Normal => match bytes[index] {
                b'"' => state = State::String,
                b'/' if bytes.get(index + 1) == Some(&b'/') => {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    state = State::LineComment;
                    index += 1;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    state = State::BlockComment;
                    index += 1;
                }
                _ => {}
            },
            State::String => {
                if escaped {
                    escaped = false;
                } else if bytes[index] == b'\\' {
                    escaped = true;
                } else if bytes[index] == b'"' {
                    state = State::Normal;
                }
            }
            State::LineComment => {
                if matches!(bytes[index], b'\n' | b'\r') {
                    state = State::Normal;
                } else {
                    bytes[index] = b' ';
                }
            }
            State::BlockComment => {
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    state = State::Normal;
                    index += 1;
                } else if !matches!(bytes[index], b'\n' | b'\r') {
                    bytes[index] = b' ';
                }
            }
        }
        index += 1;
    }

    state = State::Normal;
    escaped = false;
    for index in 0..bytes.len() {
        match state {
            State::Normal if bytes[index] == b'"' => state = State::String,
            State::Normal if bytes[index] == b',' => {
                let mut next = index + 1;
                while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                    next += 1;
                }
                if matches!(bytes.get(next), Some(b'}' | b']')) {
                    bytes[index] = b' ';
                }
            }
            State::String if escaped => escaped = false,
            State::String if bytes[index] == b'\\' => escaped = true,
            State::String if bytes[index] == b'"' => state = State::Normal,
            _ => {}
        }
    }
    String::from_utf8(bytes).expect("JSONC stripping replaces only ASCII bytes")
}
