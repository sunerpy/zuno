use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;

#[cfg(unix)]
use std::time::Instant;

const TEST_CONFIG: &str = r#"{"formatter":false,"lsp":false,"model":"test/test-model","provider":{"test":{"name":"test","id":"test","env":[],"transport":"openai-compatible","models":{"test-model":{"id":"test-model","name":"Test model","attachment":false,"reasoning":false,"temperature":false,"tool_call":true,"release_date":"2025-01-01","limit":{"context":100000,"output":10000},"cost":{"input":0,"output":0},"options":{}}},"options":{"apiKey":"acp-probe","baseURL":"https://example.invalid/v1"}}}}"#;

use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zuno"))
}

fn acp_stderr() -> Stdio {
    #[cfg(windows)]
    {
        // A session/new crash closes stdout before the request helper can
        // report a JSON-RPC error. Inherit stderr on native Windows so the
        // process-level cause is retained in the per-suite CI diagnostic log.
        Stdio::inherit()
    }
    #[cfg(not(windows))]
    {
        Stdio::piped()
    }
}

fn isolated_command(root: &std::path::Path) -> Command {
    isolated_command_with_config(root, TEST_CONFIG)
}

fn isolated_command_with_config(root: &std::path::Path, config: &str) -> Command {
    let config: Value =
        serde_json::from_str(config).expect("isolated ACP config must be valid JSON");
    let config = trusted_acp_process_config(config).to_string();
    let home = root.join("home");
    let data = root.join("data");
    let config_home = root.join("config");
    let cache = root.join("cache");
    let state = root.join("state");
    let temp = root.join("temp");
    for directory in [&home, &data, &config_home, &cache, &state, &temp] {
        std::fs::create_dir_all(directory).expect("create isolated ACP directory");
    }
    #[cfg(windows)]
    let roaming = root.join("appdata").join("roaming");
    #[cfg(windows)]
    let local = root.join("appdata").join("local");
    #[cfg(windows)]
    for directory in [&roaming, &local] {
        std::fs::create_dir_all(directory).expect("create isolated Windows ACP directory");
    }

    let mut command = binary();
    command
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache)
        .env("XDG_STATE_HOME", &state)
        .env("TMPDIR", &temp)
        .env("ZUNO_DB", root.join("zuno-acp.db"))
        .env("ZUNO_AUTH_CONTENT", "{}")
        .env("ZUNO_CONFIG_CONTENT", config)
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true");
    #[cfg(windows)]
    command
        .env("USERPROFILE", &home)
        .env("APPDATA", roaming)
        .env("LOCALAPPDATA", local)
        .env("TEMP", &temp)
        .env("TMP", &temp);
    command
}

fn trusted_acp_process_config(config: Value) -> Value {
    let mut config = zuno_testkit::trusted_platform_config(config);
    if !cfg!(target_os = "linux") {
        // These protocol tests already run inside an isolated runner. Keep the
        // production read-only Agents read-only, but hide Shell from the ones this
        // suite executes so unsupported hosts can exercise Plan and child-session
        // protocol behavior without pretending to provide a native read-only OS
        // sandbox.
        let object = config
            .as_object_mut()
            .expect("isolated ACP config must be an object");
        let agents = object
            .entry("agents")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("isolated ACP agents config must be an object");
        for (name, tools) in [
            (
                "plan",
                json!([
                    "read",
                    "glob",
                    "grep",
                    "lsp",
                    "webfetch",
                    "web_search",
                    "question",
                    "plan_exit",
                    "goal_get",
                    "plan_get",
                    "plan_update",
                    "todo_get",
                    "todo_update",
                    "skill"
                ]),
            ),
            ("explorer", json!(["read", "glob", "grep", "lsp", "skill"])),
        ] {
            agents
                .entry(name)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap_or_else(|| panic!("isolated ACP {name} config must be an object"))
                .insert("tools".to_owned(), tools);
        }
    }
    config
}

fn config_with_second_model(base_url: &str) -> String {
    let mut config: Value = serde_json::from_str(TEST_CONFIG).expect("test config JSON");
    config["provider"]["test"]["models"]["test-model-2"] = json!({
        "id": "test-model-2",
        "name": "Second test model",
        "attachment": false,
        "reasoning": false,
        "temperature": false,
        "tool_call": true,
        "release_date": "2025-01-01",
        "limit": {"context": 120000, "output": 10000},
        "cost": {"input": 0, "output": 0},
        "options": {}
    });
    config["provider"]["test"]["options"]["baseURL"] = json!(format!("{base_url}/v1"));
    serde_json::to_string(&config).expect("encode test config")
}

fn strict_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(base_url)).expect("test config JSON");
    config["permission"] = json!({"mode":"strict","rules":{}});
    serde_json::to_string(&config).expect("encode strict test config")
}

fn danger_full_access_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&strict_config(base_url)).expect("strict test config JSON");
    config["sandbox"] = json!({"mode":"danger-full-access"});
    serde_json::to_string(&config).expect("encode danger-full-access test config")
}

fn config_with_remote_mcp(mcp_url: &str, provider_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(provider_url)).expect("test config JSON");
    config["mcp"] = json!({
        "lifecycle": {
            "type": "remote",
            "url": mcp_url,
            "oauth": false
        }
    });
    serde_json::to_string(&config).expect("encode remote MCP test config")
}

async fn mount_remote_mcp_fixture(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("initialize body");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("mcp-session-id", "acp-lifecycle-session")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "acp-lifecycle-fixture", "version": "1.0.0"}
                    }
                }))
        })
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("tools/list body");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {
                        "tools": [{
                            "name": "ping",
                            "description": "Return pong.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {}
                            }
                        }]
                    }
                }))
        })
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

const ACP_MCP_SENTINEL: &str = "acp-mcp-sentinel-credential";

#[cfg(unix)]
fn write_acp_stdio_mcp_server(directory: &std::path::Path) -> std::path::PathBuf {
    let path = directory.join("acp-mcp-server.sh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
set -eu
log=${ACP_MCP_LOG:?}
printf 'start pid=%s cwd=%s\n' "$$" "$PWD" >> "$log"
if [ -n "${ACP_MCP_SECRET:-}" ]; then
  printf 'secret-ok\n' >> "$log"
else
  printf 'secret-missing\n' >> "$log"
fi
if [ "${ACP_MCP_FAIL:-0}" = "1" ]; then
  exit 23
fi
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"acp-stdio-fixture","version":"1"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"ping","description":"Return pong.","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":false,"content":[{"type":"text","text":"pong"}]}}\n' "$id"
      ;;
  esac
done
"#,
    )
    .expect("write ACP stdio MCP fixture");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("make ACP stdio MCP fixture executable");
    path
}

#[cfg(unix)]
fn acp_stdio_mcp_declaration(
    name: &str,
    script: &std::path::Path,
    log: &std::path::Path,
    fail: bool,
) -> Value {
    json!({
        "name": name,
        "command": "/bin/sh",
        "args": [script],
        "env": [
            {"name": "ACP_MCP_LOG", "value": log},
            {"name": "ACP_MCP_SECRET", "value": ACP_MCP_SENTINEL},
            {"name": "ACP_MCP_FAIL", "value": if fail { "1" } else { "0" }}
        ]
    })
}

#[cfg(unix)]
fn wait_for_occurrences(path: &std::path::Path, needle: &str, expected: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.matches(needle).count() >= expected {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} `{needle}` entries in {}: {text:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn started_pids(text: &str) -> Vec<u32> {
    text.lines()
        .filter(|line| line.starts_with("start "))
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|field| field.strip_prefix("pid="))
        })
        .map(|pid| pid.parse::<u32>().expect("fixture pid is numeric"))
        .collect()
}

#[cfg(unix)]
fn wait_for_process_exit(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let alive = Command::new("/bin/kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !alive {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "MCP child process {pid} remained alive after disposal"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_file_tree_excludes(root: &std::path::Path, secret: &str) {
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "secret was persisted in {}",
            entry.path().display()
        );
    }
}

fn reasoning_config() -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model("https://example.invalid"))
            .expect("test config JSON");
    let model = &mut config["provider"]["test"]["models"]["test-model"];
    model["reasoning"] = json!(true);
    model["variants"] = json!({
        "low": {"reasoningEffort": "low"},
        "xhigh": {"reasoningEffort": "xhigh"},
        "max": {"reasoningEffort": "max"}
    });
    serde_json::to_string(&config).expect("encode reasoning test config")
}

fn rich_prompt_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(base_url)).expect("test config JSON");
    let model = &mut config["provider"]["test"]["models"]["test-model"];
    model["attachment"] = json!(true);
    model["modalities"] = json!({
        "input": ["text", "image"],
        "output": ["text"]
    });
    serde_json::to_string(&config).expect("encode rich prompt test config")
}

fn manual_compaction_config(base_url: &str) -> String {
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(base_url)).expect("test config JSON");
    config["small_model"] = json!("test/test-model");
    config["compaction"] = json!({
        "auto": false,
        "tail_turns": 1,
        "preserve_recent_tokens": 1_000
    });
    serde_json::to_string(&config).expect("encode compaction test config")
}

fn put_durable_message(
    connection: &zuno_db::Connection,
    session_id: &str,
    id: &str,
    role: &str,
    created: i64,
    extra: Value,
) {
    let mut payload = json!({
        "id": id,
        "sessionID": session_id,
        "role": role,
        "time": { "created": created },
    });
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            payload[key] = value;
        }
    }
    let record = zuno_db::message::MessageRecord::from_json(payload).expect("durable message");
    zuno_db::message::MessageStore::new(connection)
        .put_message_at(&record, created)
        .expect("persist durable message");
}

fn put_durable_part(
    connection: &zuno_db::Connection,
    session_id: &str,
    message_id: &str,
    id: &str,
    created: i64,
    body: Value,
) {
    let mut payload = json!({
        "id": id,
        "sessionID": session_id,
        "messageID": message_id,
    });
    if let Value::Object(body) = body {
        for (key, value) in body {
            payload[key] = value;
        }
    }
    let record = zuno_db::message::PartRecord::from_json(payload, created).expect("durable part");
    zuno_db::message::MessageStore::new(connection)
        .put_part_at(&record, created)
        .expect("persist durable part");
}

fn seed_durable_replay(root: &std::path::Path, session_id: &str) {
    let location = zuno_paths::DbLocation::File(root.join("zuno-acp.db"));
    let connection = zuno_db::open::open(&location).expect("open ACP database");
    let user_id = "msg_acp_load_user";
    put_durable_message(&connection, session_id, user_id, "user", 100, Value::Null);
    put_durable_part(
        &connection,
        session_id,
        user_id,
        "prt_acp_load_user_01_text",
        100,
        json!({"type":"text","text":"replay this durable session"}),
    );
    put_durable_part(
        &connection,
        session_id,
        user_id,
        "prt_acp_load_user_02_image",
        101,
        json!({
            "type": "file",
            "filename": "pixel.png",
            "mime": "image/png",
            "data": "aGVsbG8=",
            "url": "data:image/png;base64,aGVsbG8=",
        }),
    );

    let assistant_id = "msg_acp_load_assistant";
    put_durable_message(
        &connection,
        session_id,
        assistant_id,
        "assistant",
        200,
        json!({
            "cost": 1.25,
            "finish": "stop",
            "tokens": {
                "input": 100,
                "output": 25,
                "reasoning": 0,
                "cache": {"read": 40, "write": 10},
                "accounting": "cache-beside-input"
            }
        }),
    );
    put_durable_part(
        &connection,
        session_id,
        assistant_id,
        "prt_acp_load_assistant_01_thought",
        200,
        json!({"type":"reasoning","text":"inspect durable state"}),
    );
    let edited_path = root.join("src/lib.rs");
    std::fs::create_dir_all(edited_path.parent().expect("edited path parent"))
        .expect("create replay source directory");
    std::fs::write(&edited_path, "new\n").expect("write replay source file");
    let edited_path = edited_path.to_string_lossy().into_owned();
    put_durable_part(
        &connection,
        session_id,
        assistant_id,
        "prt_acp_load_assistant_02_tool",
        201,
        json!({
            "type": "tool",
            "callID": "call_acp_load_edit",
            "tool": "edit",
            "displayName": "Edit lib.rs",
            "state": {
                "status": "completed",
                "raw": serde_json::to_string(&json!({"filePath": edited_path})).expect("raw input"),
                "input": {"filePath": edited_path},
                "title": "Updated lib.rs",
                "output": "ok",
                "metadata": {
                    "diff": "@@ -1 +1 @@\n-old\n+new\n",
                    "fileDiffs": [{"path": edited_path, "oldText": "old\n", "newText": "new\n"}],
                    "writtenPaths": [edited_path]
                },
                "attachments": [{
                    "mime": "image/png",
                    "filename": "preview.png",
                    "url": "data:image/png;base64,cHJldmlldw=="
                }]
            }
        }),
    );
    put_durable_part(
        &connection,
        session_id,
        assistant_id,
        "prt_acp_load_assistant_03_text",
        202,
        json!({"type":"text","text":"durable replay complete"}),
    );
    connection
        .execute(
            "UPDATE session SET cost = ?2 WHERE id = ?1",
            rusqlite::params![session_id, 1.25_f64],
        )
        .expect("persist session cost");
    drop(connection);

    let pool = Arc::new(zuno_db::Pool::open(&location).expect("open ACP work-state pool"));
    zuno_tools::work_state::WorkStateStore::new(pool)
        .update_plan(
            session_id,
            zuno_tools::work_state::PlanUpdateParams {
                expected_revision: None,
                goal_id: None,
                title: "Verify ACP load".to_owned(),
                steps: vec![
                    zuno_tools::work_state::PlanStep {
                        id: "replay".to_owned(),
                        title: "Replay durable state".to_owned(),
                        status: zuno_tools::work_state::PlanStepStatus::InProgress,
                    },
                    zuno_tools::work_state::PlanStep {
                        id: "verify".to_owned(),
                        title: "Verify Zed projection".to_owned(),
                        status: zuno_tools::work_state::PlanStepStatus::Pending,
                    },
                ],
            },
        )
        .expect("persist ACP work plan");
}

fn seed_durable_text_replay(root: &std::path::Path, session_id: &str) {
    let location = zuno_paths::DbLocation::File(root.join("zuno-acp.db"));
    let connection = zuno_db::open::open(&location).expect("open ACP database");
    put_durable_message(
        &connection,
        session_id,
        "msg_acp_cold_load",
        "user",
        100,
        Value::Null,
    );
    put_durable_part(
        &connection,
        session_id,
        "msg_acp_cold_load",
        "prt_acp_cold_load",
        100,
        json!({"type":"text","text":"cold-load durable history"}),
    );
}

fn seed_durable_child_replay(
    root: &std::path::Path,
    parent_session_id: &str,
    child_session_id: &str,
) {
    let location = zuno_paths::DbLocation::File(root.join("zuno-acp.db"));
    let pool = zuno_db::Pool::open(&location).expect("open ACP child replay pool");
    let store = zuno_db::session::Store::new(&pool);
    let parent = store
        .get(parent_session_id)
        .expect("load ACP replay parent");
    let mut child = zuno_db::session::SessionCreate::new(
        child_session_id,
        "child-replay",
        parent.project_id,
        root.to_string_lossy(),
        parent.directory,
        "Inspect child history",
        env!("CARGO_PKG_VERSION"),
    )
    .with_parent(parent_session_id)
    .at(300);
    child.agent = Some("explorer".to_owned());
    child.model = Some(
        serde_json::to_string(&json!({
            "providerID": "test",
            "id": "test-model"
        }))
        .expect("child model"),
    );
    store.create(&child).expect("create ACP replay child");

    let connection = pool.get().expect("open ACP child replay connection");
    put_durable_message(
        &connection,
        child_session_id,
        "msg_child_user",
        "user",
        310,
        Value::Null,
    );
    put_durable_part(
        &connection,
        child_session_id,
        "msg_child_user",
        "prt_child_user",
        310,
        json!({"type":"text","text":"inspect the child tree"}),
    );
    put_durable_message(
        &connection,
        child_session_id,
        "msg_child_assistant",
        "assistant",
        320,
        json!({
            "tokens": {
                "input": 40,
                "output": 10,
                "reasoning": 0,
                "cache": {"read": 0, "write": 0},
                "accounting": "cache-inside-input"
            }
        }),
    );
    put_durable_part(
        &connection,
        child_session_id,
        "msg_child_assistant",
        "prt_child_assistant",
        320,
        json!({"type":"text","text":"child history restored"}),
    );
}

fn seed_compactable_history(root: &std::path::Path, session_id: &str) {
    let location = zuno_paths::DbLocation::File(root.join("zuno-acp.db"));
    let connection = zuno_db::open::open(&location).expect("open ACP database");
    for turn in 0_i64..4 {
        let created = 1_000 + turn * 10;
        let user_id = format!("msg_compact_user_{turn}");
        put_durable_message(
            &connection,
            session_id,
            &user_id,
            "user",
            created,
            Value::Null,
        );
        put_durable_part(
            &connection,
            session_id,
            &user_id,
            &format!("prt_compact_user_{turn}"),
            created,
            json!({"type":"text","text":format!("user request {turn}")}),
        );

        let assistant_id = format!("msg_compact_assistant_{turn}");
        put_durable_message(
            &connection,
            session_id,
            &assistant_id,
            "assistant",
            created + 1,
            json!({
                "providerID": "test",
                "modelID": "test-model",
                "finish": "stop",
                "cost": 0.0,
                "tokens": {
                    "input": 20,
                    "output": 5,
                    "reasoning": 0,
                    "cache": {"read": 0, "write": 0},
                    "accounting": "cache-inside-input"
                }
            }),
        );
        put_durable_part(
            &connection,
            session_id,
            &assistant_id,
            &format!("prt_compact_assistant_{turn}"),
            created + 1,
            json!({"type":"text","text":format!("assistant response {turn}")}),
        );
    }
}

#[test]
fn acp_initialize_uses_stable_v1_without_fake_authentication() {
    let root = tempfile::tempdir().expect("ACP test root");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientInfo": {
                "name": "zuno-test",
                "title": "Zuno ACP integration test",
                "version": "0"
            },
            "clientCapabilities": {
                "fs": {
                    "readTextFile": true,
                    "writeTextFile": true
                },
                "terminal": true
            }
        }
    });
    writeln!(
        child.stdin.as_mut().expect("ACP stdin"),
        "{}",
        serde_json::to_string(&request).expect("encode initialize")
    )
    .expect("write initialize");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for zuno acp");
    assert!(
        output.status.success(),
        "ACP process failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("ACP stdout is UTF-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        1,
        "stdout must contain exactly one JSON-RPC frame, not banners or logs: {stdout:?}"
    );
    let response: Value = serde_json::from_str(lines[0]).expect("ACP response JSON");
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["protocolVersion"], 1);
    assert_eq!(response["result"]["agentInfo"]["name"], "zuno");
    assert!(
        response["result"]["authMethods"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "Zuno must not advertise a synthetic ACP login: {response}"
    );
    assert_eq!(response["result"]["agentCapabilities"]["loadSession"], true);
    assert!(response["result"]["agentCapabilities"]["sessionCapabilities"]["list"].is_object());
    assert!(response["result"]["agentCapabilities"]["sessionCapabilities"]["resume"].is_object());
    assert_eq!(
        response["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        true
    );
    assert_eq!(
        response["result"]["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
        true
    );
    assert_eq!(
        response["result"]["agentCapabilities"]["mcpCapabilities"],
        json!({"stdio": true, "http": true, "sse": false})
    );
}

fn request(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP response");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return response["result"].clone();
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected ACP frame while waiting for {method}: {response}"
        );
    }
}

fn request_failure(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP response");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            return response.get("error").cloned().unwrap_or_else(|| {
                panic!("ACP request {method} unexpectedly succeeded: {response}")
            });
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected ACP frame while waiting for {method}: {response}"
        );
    }
}

fn read_session_update(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut line = String::new();
    stdout
        .read_line(&mut line)
        .expect("read ACP session update");
    assert!(
        !line.is_empty(),
        "ACP closed before publishing a session update"
    );
    let frame: Value = serde_json::from_str(&line).expect("ACP session update JSON");
    assert_eq!(
        frame.get("method").and_then(Value::as_str),
        Some("session/update"),
        "expected an ACP session update: {frame}"
    );
    frame["params"].clone()
}

fn request_with_updates(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return (response["result"].clone(), updates);
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected frame while waiting for {method}: {response}"
        );
        let update = response["params"]["update"].clone();
        if update["sessionUpdate"] != "available_commands_update" {
            updates.push(update);
        }
    }
}

fn request_with_all_updates(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return (response["result"].clone(), updates);
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected frame while waiting for {method}: {response}"
        );
        updates.push(response["params"]["update"].clone());
    }
}

fn request_with_routed_updates(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return (response["result"].clone(), updates);
        }
        assert_eq!(
            response.get("method").and_then(Value::as_str),
            Some("session/update"),
            "unexpected frame while waiting for {method}: {response}"
        );
        if response["params"]["update"]["sessionUpdate"] != "available_commands_update" {
            updates.push(response["params"].clone());
        }
    }
}

fn request_with_elicitation(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
    expected_session_id: &str,
) -> (Value, Vec<Value>, Value) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    let mut elicitation = None;
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            let elicitation = elicitation.unwrap_or_else(|| {
                panic!(
                    "{method} completed without elicitation/create: response={response:#} updates={updates:#?}"
                )
            });
            return (response["result"].clone(), updates, elicitation);
        }

        match response.get("method").and_then(Value::as_str) {
            Some("session/update") => updates.push(response["params"]["update"].clone()),
            Some("elicitation/create") => {
                assert!(
                    elicitation.is_none(),
                    "duplicate elicitation request: {response}"
                );
                assert_eq!(response["jsonrpc"], "2.0");
                let request_id = response
                    .get("id")
                    .filter(|request_id| request_id.is_string())
                    .cloned()
                    .expect("elicitation request id must be a string");
                let request = response["params"].clone();
                assert_eq!(request["sessionId"], expected_session_id);
                assert_eq!(request["toolCallId"], "call_question");
                assert_eq!(request["mode"], "form");
                assert_eq!(request["message"], "Which database?");

                let schema = &request["requestedSchema"];
                assert_eq!(schema["type"], "object");
                assert_eq!(schema["title"], "Questions");
                assert!(
                    schema.get("required").is_none(),
                    "choice plus custom answer fields are alternatives"
                );
                assert_eq!(
                    schema["properties"]
                        .as_object()
                        .expect("form properties")
                        .len(),
                    2
                );
                let choice = &schema["properties"]["q0_choice"];
                assert_eq!(choice["type"], "string");
                assert_eq!(choice["title"], "Database");
                assert_eq!(choice["description"], "Which database?");
                let options = choice["oneOf"].as_array().expect("native choices");
                assert_eq!(options.len(), 2);
                assert_eq!(options[0]["const"], "Postgres");
                assert_eq!(options[0]["description"], "Relational database");
                assert_eq!(options[1]["const"], "SQLite");
                assert_eq!(options[1]["description"], "Embedded database");

                let custom = &schema["properties"]["q0_custom"];
                assert_eq!(custom["type"], "string");
                assert_eq!(custom["title"], "Database — Other");
                assert_eq!(custom["minLength"], 1);
                assert!(custom["description"].as_str().is_some_and(|description| {
                    description.contains("Which database?") && description.contains("custom answer")
                }));

                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "action": "accept",
                        "content": {"q0_choice": "SQLite"}
                    }
                });
                writeln!(
                    stdin,
                    "{}",
                    serde_json::to_string(&reply).expect("encode elicitation response")
                )
                .expect("write elicitation response");
                stdin.flush().expect("flush elicitation response");
                elicitation = Some(request);
            }
            _ => panic!("unexpected frame while waiting for {method}: {response}"),
        }
    }
}

fn request_with_permissions(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    id: u64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>, Vec<Value>) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    writeln!(
        stdin,
        "{}",
        serde_json::to_string(&frame).expect("encode ACP request")
    )
    .expect("write ACP request");
    stdin.flush().expect("flush ACP request");

    let mut updates = Vec::new();
    let mut permissions = Vec::new();
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read ACP frame");
        assert!(!line.is_empty(), "ACP closed before responding to {method}");
        let response: Value = serde_json::from_str(&line).expect("ACP response JSON");
        if response.get("id") == Some(&json!(id)) {
            assert!(
                response.get("error").is_none(),
                "ACP request {method} failed: {response}"
            );
            return (response["result"].clone(), updates, permissions);
        }
        match response.get("method").and_then(Value::as_str) {
            Some("session/update") => updates.push(response["params"]["update"].clone()),
            Some("session/request_permission") => {
                let request_id = response.get("id").cloned().expect("permission request id");
                let request = response["params"].clone();
                permissions.push(request);
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "outcome": {
                            "outcome": "selected",
                            "optionId": "allow_once"
                        }
                    }
                });
                writeln!(
                    stdin,
                    "{}",
                    serde_json::to_string(&reply).expect("encode permission response")
                )
                .expect("write permission response");
                stdin.flush().expect("flush permission response");
            }
            _ => panic!("unexpected frame while waiting for {method}: {response}"),
        }
    }
}

#[derive(Clone)]
struct TextTurnResponder;

impl Respond for TextTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        compatible_text_response(if has_tools { "ACP reply" } else { "ACP title" })
    }
}

#[derive(Clone, Default)]
struct GoalCompletionTurnResponder {
    tool_requests: Arc<AtomicUsize>,
}

impl Respond for GoalCompletionTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP goal title");
        }
        let request_index = self.tool_requests.fetch_add(1, Ordering::SeqCst);
        if request_index.is_multiple_of(2) {
            compatible_tool_response(
                &format!("call_goal_complete_{}", request_index / 2 + 1),
                "goal_update",
                json!({
                    "expected_revision": 1,
                    "status": "complete"
                }),
            )
        } else {
            compatible_text_response("Goal completed automatically")
        }
    }
}

fn compatible_text_response(text: &str) -> ResponseTemplate {
    let chunk = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
    let finish = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11}});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

fn compatible_tool_response(call_id: &str, name: &str, arguments: Value) -> ResponseTemplate {
    let arguments = serde_json::to_string(&arguments).expect("serialize tool arguments");
    let call = json!({
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "function": {"name": name, "arguments": arguments}
                }]
            },
            "finish_reason": null
        }]
    });
    let finish = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
    });
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

fn tool_result_json(messages: &[Value], call_id: &str) -> Option<Value> {
    messages
        .iter()
        .find(|message| {
            message["role"] == "tool" && message["tool_call_id"].as_str() == Some(call_id)
        })
        .and_then(|message| message["content"].as_str())
        .and_then(|content| serde_json::from_str(content).ok())
}

#[derive(Clone)]
struct QuestionTurnResponder;

impl Respond for QuestionTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP question title");
        }

        let has_question_result =
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "call_question"
                    })
                });
        if has_question_result {
            compatible_text_response("Configured SQLite")
        } else {
            compatible_tool_response(
                "call_question",
                "question",
                json!({
                    "questions": [{
                        "question": "Which database?",
                        "header": "Database",
                        "options": [
                            {"label": "Postgres", "description": "Relational database"},
                            {"label": "SQLite", "description": "Embedded database"}
                        ]
                    }]
                }),
            )
        }
    }
}

#[derive(Clone)]
struct SubagentTurnResponder;

impl Respond for SubagentTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP subagent title");
        }
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let child_turn = messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("Inspect child live stream.")
        });
        if child_turn {
            return compatible_text_response("child live result");
        }
        let has_task_result = messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call_task");
        if has_task_result {
            compatible_text_response("parent observed child")
        } else {
            compatible_tool_response(
                "call_task",
                "task",
                json!({
                    "objective": "Inspect live child",
                    "deliverable": "A concise inspection result.",
                    "instructions": "Inspect child live stream.",
                    "success_evidence": "Return the inspection result in the child session.",
                    "agent": "explorer",
                    "background": false
                }),
            )
        }
    }
}

#[derive(Clone)]
struct ParentReportsChildBlockerTurnResponder;

impl Respond for ParentReportsChildBlockerTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP child question title");
        }
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let child_turn = messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("Ask child for database.")
        });
        if child_turn {
            let advertised_tools = body["tools"]
                .as_array()
                .expect("provider tools")
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str())
                .collect::<Vec<_>>();
            assert!(
                !advertised_tools.contains(&"question"),
                "delegated children must report blockers instead of asking users directly: \
                 {advertised_tools:#?}"
            );
            assert!(
                messages
                    .iter()
                    .filter(|message| message["role"] == "tool")
                    .all(|message| message["tool_call_id"] != "call_question"),
                "the delegated child must not receive a synthetic question result: {messages:#?}"
            );
            return compatible_text_response("child needs parent to ask which database");
        }
        if let Some(child_report) = messages.iter().find(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "call_task_child_question"
        }) {
            assert!(
                child_report["content"]
                    .to_string()
                    .contains("child needs parent to ask which database"),
                "delegated child must report its blocker to the parent: {child_report:#}"
            );
            return compatible_text_response("Which database should be used: Postgres or SQLite?");
        }
        compatible_tool_response(
            "call_task_child_question",
            "task",
            json!({
                "objective": "Ask child question",
                "deliverable": "The selected database.",
                "instructions": "Ask child for database.",
                "success_evidence": "Report the blocker so the parent can ask the user.",
                "agent": "deep",
                "background": false
            }),
        )
    }
}

#[derive(Clone)]
struct SubagentWriteTurnResponder {
    path: String,
}

impl Respond for SubagentWriteTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP child write title");
        }
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let child_turn = messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("Create the delegated file.")
        });
        if child_turn {
            let answered = messages.iter().any(|message| {
                message["role"] == "tool" && message["tool_call_id"] == "call_child_write"
            });
            return if answered {
                compatible_text_response("child created the file")
            } else {
                compatible_tool_response(
                    "call_child_write",
                    "write",
                    json!({
                        "filePath": self.path,
                        "content": "created by ACP child\n"
                    }),
                )
            };
        }
        let has_task_result = messages.iter().any(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "call_task_child_write"
        });
        if has_task_result {
            compatible_text_response("parent received child write")
        } else {
            compatible_tool_response(
                "call_task_child_write",
                "task",
                json!({
                    "objective": "Create delegated file",
                    "deliverable": "The requested file on disk.",
                    "instructions": "Create the delegated file.",
                    "success_evidence": "The file exists with the requested content.",
                    "agent": "deep",
                    "background": false
                }),
            )
        }
    }
}

#[derive(Clone)]
struct BackgroundSubagentTurnResponder;

impl Respond for BackgroundSubagentTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP background child title");
        }
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let child_turn = messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("Wait until the parent session closes.")
        });
        if child_turn {
            return compatible_text_response("background child unexpectedly completed")
                .set_delay(Duration::from_secs(30));
        }
        let has_task_result = messages.iter().any(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "call_background_task"
        });
        if has_task_result {
            compatible_text_response("background child started")
        } else {
            compatible_tool_response(
                "call_background_task",
                "task",
                json!({
                    "objective": "Wait for parent close",
                    "deliverable": "A cancellation lifecycle record.",
                    "instructions": "Wait until the parent session closes.",
                    "success_evidence": "The job is cancelled when the parent closes.",
                    "agent": "deep",
                    "background": true,
                    "reportDelivery": "quiet"
                }),
            )
        }
    }
}

#[derive(Clone)]
struct CompletingBackgroundSubagentTurnResponder;

impl Respond for CompletingBackgroundSubagentTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP completing background child title");
        }
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let transcript = serde_json::to_string(&messages).expect("provider messages JSON");
        let child_turn = messages.iter().any(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("Finish after the parent has returned its waiting reply.")
        });
        if child_turn {
            return compatible_text_response(
                "background child completed after the parent turn ended",
            )
            .set_delay(Duration::from_secs(1));
        }
        if transcript.contains("background child completed after the parent turn ended") {
            if messages.iter().any(|message| {
                message["role"] == "tool"
                    && message["tool_call_id"] == "call_complete_background_plan"
            }) {
                return compatible_text_response(
                    "parent resumed after background child completion",
                );
            }
            let plan = tool_result_json(&messages, "call_background_plan")
                .expect("background parent must retain its created Plan");
            let revision = plan["revision"]
                .as_i64()
                .expect("created background Plan revision");
            let step_id = plan["steps"][0]["id"]
                .as_str()
                .expect("created background Plan step ID");
            return compatible_tool_response(
                "call_complete_background_plan",
                "plan_update",
                json!({
                    "action": "patch",
                    "expected_revision": revision,
                    "steps": [{
                        "id": step_id,
                        "status": "completed"
                    }]
                }),
            );
        }
        if tool_result_json(&messages, "call_background_plan").is_none() {
            return compatible_tool_response(
                "call_background_plan",
                "plan_update",
                json!({
                    "action": "create",
                    "title": "Track background completion",
                    "steps": [{
                        "title": "Wait for and integrate the background result",
                        "status": "in_progress"
                    }]
                }),
            );
        }
        let has_task_result = messages.iter().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == "call_completing_background_task"
        });
        if has_task_result {
            compatible_text_response("Background task is still running; I will wait.")
        } else {
            compatible_tool_response(
                "call_completing_background_task",
                "task",
                json!({
                    "objective": "Complete after the parent turn",
                    "deliverable": "A result delivered to a later parent turn.",
                    "instructions": "Finish after the parent has returned its waiting reply.",
                    "success_evidence": "The parent emits a new reply without another user prompt.",
                    "agent": "deep",
                    "background": true,
                    "reportDelivery": "nextStep"
                }),
            )
        }
    }
}

#[derive(Clone)]
struct PlanTurnResponder;

impl Respond for PlanTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP plan title");
        }
        let has_plan_result =
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "call_plan"
                    })
                });
        if has_plan_result {
            compatible_text_response("Plan recorded")
        } else {
            compatible_tool_response(
                "call_plan",
                "plan_update",
                json!({
                    "action": "create",
                    "title": "Verify ACP projection",
                    "steps": [
                        {"title":"Inspect ACP plan projection","status":"completed"},
                        {"title":"Verify Zed update","status":"completed"}
                    ]
                }),
            )
        }
    }
}

#[derive(Clone)]
struct WriteTurnResponder {
    path: String,
}

impl Respond for WriteTurnResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        if !has_tools {
            return compatible_text_response("ACP write title");
        }
        let has_write_result =
            body.get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "call_write"
                    })
                });
        if has_write_result {
            compatible_text_response("File created")
        } else {
            compatible_tool_response(
                "call_write",
                "write",
                json!({"filePath":self.path,"content":"created through ACP\n"}),
            )
        }
    }
}

#[test]
fn acp_session_lifecycle_uses_the_durable_zuno_store() {
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("acp-check.md"),
        "Inspect the ACP integration for $ARGUMENTS.",
    )
    .expect("write project command");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let cwd = root.path().to_string_lossy().into_owned();
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("new session id")
        .to_owned();
    assert_eq!(created["modes"]["currentModeId"], "build");
    assert!(
        created["configOptions"]
            .as_array()
            .is_some_and(|options| options.iter().any(|option| option["id"] == "model")),
        "new session must expose its resolved model selector: {created}"
    );
    assert!(
        created["configOptions"]
            .as_array()
            .and_then(|options| options.iter().find(|option| option["id"] == "agent"))
            .and_then(|option| option["options"].as_array())
            .is_some_and(|agents| agents.iter().any(|agent| agent["value"] == "plan")),
        "new session must expose plan in the Agent selector: {created}"
    );
    let commands = read_session_update(&mut stdout);
    assert_eq!(commands["sessionId"], session_id);
    let command_update = &commands["update"];
    assert_eq!(command_update["sessionUpdate"], "available_commands_update");
    assert!(
        command_update["availableCommands"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command["name"] == "acp-check")),
        "project commands must be published after session/new: {command_update}"
    );
    let compact = command_update["availableCommands"]
        .as_array()
        .expect("available commands")
        .iter()
        .filter(|command| command["name"] == "compact")
        .collect::<Vec<_>>();
    assert_eq!(compact.len(), 1, "compact must be advertised exactly once");
    assert_eq!(
        compact[0]["description"],
        "Summarize older context and keep the recent turn tail"
    );
    assert!(
        compact[0].get("input").is_none(),
        "native /compact does not accept prompt arguments"
    );
    let skill_directory = root.path().join(".agents/skills/spreadsheet");
    std::fs::create_dir_all(&skill_directory).expect("create live Skill directory");
    std::fs::write(
        skill_directory.join("SKILL.md"),
        "---\nname: spreadsheet\ndescription: Edit spreadsheet workbooks.\n---\nHandle sheets.\n",
    )
    .expect("install live Skill");
    let mut projected = None;
    for id in 100..130 {
        std::thread::sleep(Duration::from_millis(100));
        let (_, updates) = request_with_all_updates(
            &mut stdin,
            &mut stdout,
            id,
            "session/list",
            json!({"cwd": root.path()}),
        );
        projected = updates.into_iter().find(|update| {
            update["sessionUpdate"] == "available_commands_update"
                && update["availableCommands"]
                    .as_array()
                    .is_some_and(|commands| {
                        commands
                            .iter()
                            .any(|command| command["name"] == "spreadsheet")
                    })
        });
        if projected.is_some() {
            break;
        }
    }
    assert!(
        projected.is_some(),
        "an active ACP session did not project the newly installed Skill"
    );
    let selected_plan = request(
        &mut stdin,
        &mut stdout,
        3,
        "session/set_config_option",
        json!({"sessionId": session_id, "configId": "agent", "value": "plan"}),
    );
    assert!(
        selected_plan["configOptions"]
            .as_array()
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option["id"] == "agent" && option["currentValue"] == "plan")
            })
    );
    let plan_updates = (0..3)
        .map(|_| read_session_update(&mut stdout)["update"].clone())
        .collect::<Vec<_>>();
    assert!(plan_updates.iter().any(|update| {
        update["sessionUpdate"] == "current_mode_update" && update["currentModeId"] == "plan"
    }));
    assert!(plan_updates.iter().any(|update| {
        update["sessionUpdate"] == "config_option_update"
            && update["configOptions"].as_array().is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option["id"] == "agent" && option["currentValue"] == "plan")
            })
    }));

    let selected_deep = request(
        &mut stdin,
        &mut stdout,
        4,
        "session/set_config_option",
        json!({"sessionId": session_id, "configId": "agent", "value": "deep"}),
    );
    assert!(
        selected_deep["configOptions"]
            .as_array()
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option["id"] == "agent" && option["currentValue"] == "deep")
            })
    );
    let deep_updates = (0..3)
        .map(|_| read_session_update(&mut stdout)["update"].clone())
        .collect::<Vec<_>>();
    assert!(deep_updates.iter().any(|update| {
        update["sessionUpdate"] == "current_mode_update" && update["currentModeId"] == "build"
    }));
    assert!(deep_updates.iter().any(|update| {
        update["sessionUpdate"] == "config_option_update"
            && update["configOptions"].as_array().is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option["id"] == "agent" && option["currentValue"] == "deep")
            })
    }));

    let planned = request(
        &mut stdin,
        &mut stdout,
        5,
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "plan"}),
    );
    assert_eq!(planned, json!({}));
    let mode_plan_updates = (0..3)
        .map(|_| read_session_update(&mut stdout)["update"].clone())
        .collect::<Vec<_>>();
    assert!(mode_plan_updates.iter().any(|update| {
        update["sessionUpdate"] == "config_option_update"
            && update["configOptions"].as_array().is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option["id"] == "agent" && option["currentValue"] == "plan")
            })
    }));

    let building = request(
        &mut stdin,
        &mut stdout,
        6,
        "session/set_mode",
        json!({"sessionId": session_id, "modeId": "build"}),
    );
    assert_eq!(building, json!({}));
    let mode_build_updates = (0..3)
        .map(|_| read_session_update(&mut stdout)["update"].clone())
        .collect::<Vec<_>>();
    assert!(mode_build_updates.iter().any(|update| {
        update["sessionUpdate"] == "config_option_update"
            && update["configOptions"].as_array().is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option["id"] == "agent" && option["currentValue"] == "deep")
            })
    }));

    let listed = request(
        &mut stdin,
        &mut stdout,
        7,
        "session/list",
        json!({"cwd": root.path()}),
    );
    assert!(
        listed["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions
                .iter()
                .any(|session| { session["sessionId"] == session_id && session["cwd"] == cwd })),
        "materialized ACP session missing from durable list: {listed}"
    );

    request(
        &mut stdin,
        &mut stdout,
        8,
        "session/close",
        json!({"sessionId": session_id}),
    );
    let after_close = request(
        &mut stdin,
        &mut stdout,
        8,
        "session/list",
        json!({"cwd": root.path()}),
    );
    assert!(after_close["sessions"].as_array().is_some_and(|sessions| {
        sessions
            .iter()
            .any(|session| session["sessionId"] == session_id)
    }));

    request(
        &mut stdin,
        &mut stdout,
        9,
        "session/delete",
        json!({"sessionId": session_id, "cleanupDerivedExperiences": false}),
    );
    let after_delete = request(
        &mut stdin,
        &mut stdout,
        10,
        "session/list",
        json!({"cwd": root.path()}),
    );
    assert!(after_delete["sessions"].as_array().is_some_and(|sessions| {
        sessions
            .iter()
            .all(|session| session["sessionId"] != session_id)
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP lifecycle process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP lifecycle process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_compact_is_native_and_persists_a_summary_without_model_prompt_dispatch() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("compact.md"),
        "This user command must never shadow native compaction.",
    )
    .expect("write colliding project command");
    let config = manual_compaction_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let commands = read_session_update(&mut stdout);
    let compact = commands["update"]["availableCommands"]
        .as_array()
        .expect("available commands")
        .iter()
        .filter(|command| command["name"] == "compact")
        .collect::<Vec<_>>();
    assert_eq!(compact.len(), 1);
    assert_eq!(
        compact[0]["description"],
        "Summarize older context and keep the recent turn tail"
    );

    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    seed_compactable_history(root.path(), &session_id);
    let (_loaded, _replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let (completed, _updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        5,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"/compact"}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert_eq!(
        received.len(),
        1,
        "native compaction must make exactly its summary request"
    );
    let request_body = String::from_utf8_lossy(&received[0].body);
    assert!(
        !request_body.contains("/compact"),
        "native command leaked into the model prompt: {request_body}"
    );
    assert!(
        request_body.contains("## Objective"),
        "compaction summary prompt was not used: {request_body}"
    );

    let connection = zuno_db::open::open(&zuno_paths::DbLocation::File(
        root.path().join("zuno-acp.db"),
    ))
    .expect("open compacted ACP database");
    let history = zuno_db::message::MessageStore::new(&connection)
        .hydrate_session(&session_id)
        .expect("hydrate compacted session");
    assert!(
        history
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| { part.kind == zuno_db::message::PartKind::Compaction })
    );
    assert!(history.iter().any(|message| {
        message.info.data.get("summary") == Some(&json!(true))
            && message.info.data.get("finish") == Some(&json!("stop"))
            && message
                .parts
                .iter()
                .any(|part| part.data.get("text") == Some(&json!("ACP title")))
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP compact process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP compact process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_goal_and_plan_commands_are_native_and_do_not_enter_model_input() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(GoalCompletionTurnResponder::default())
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("temporary ACP root");
    let config = danger_full_access_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let commands = read_session_update(&mut stdout);
    let native = commands["update"]["availableCommands"]
        .as_array()
        .expect("available commands")
        .iter()
        .take(7)
        .map(|command| command["name"].as_str().expect("command name"))
        .collect::<Vec<_>>();
    assert_eq!(
        native,
        [
            "compact",
            "goal",
            "learn",
            "plan",
            "reflect",
            "start-plan",
            "start-work"
        ]
    );
    let goal_command = commands["update"]["availableCommands"]
        .as_array()
        .expect("available commands")
        .iter()
        .find(|command| command["name"] == "goal")
        .expect("goal command");
    assert_eq!(goal_command["input"]["hint"], "objective | action [value]");

    let (goal, goal_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{
                "type":"text",
                "text":"/goal What is GOAL-ONE?"
            }]
        }),
    );
    assert_eq!(goal["stopReason"], "end_turn");
    assert!(goal_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("What is GOAL-ONE?"))
    }));
    assert!(goal_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"] == "Goal completed automatically"
    }));

    let (edited, edited_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{
                "type":"text",
                "text":"/goal What is GOAL-TWO?"
            }]
        }),
    );
    assert_eq!(edited["stopReason"], "end_turn");
    assert!(edited_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("What is GOAL-TWO?"))
    }));
    assert!(edited_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"] == "Goal completed automatically"
    }));

    let (shown, shown_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        5,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"/goal show"}]
        }),
    );
    assert_eq!(shown["stopReason"], "end_turn");
    assert!(shown_updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("What is GOAL-TWO?"))
    }));

    let invalid = request_failure(
        &mut stdin,
        &mut stdout,
        6,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"/goal create"}]
        }),
    );
    assert_eq!(invalid["code"], -32602);
    assert_eq!(invalid["message"], "usage: /goal create <objective>");

    let (plan, plan_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        7,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"/plan"}]
        }),
    );
    assert_eq!(plan["stopReason"], "end_turn");
    assert!(plan_updates.iter().any(|update| {
        update["sessionUpdate"] == "current_mode_update" && update["currentModeId"] == "plan"
    }));
    assert!(plan_updates.iter().any(|update| {
        update["sessionUpdate"] == "config_option_update" && update["configOptions"].is_array()
    }));

    request(
        &mut stdin,
        &mut stdout,
        8,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP native command process failed: {stderr}");
    }

    let location = zuno_paths::DbLocation::File(root.path().join("zuno-acp.db"));
    let pool = Arc::new(zuno_db::Pool::open(&location).expect("open ACP command store"));
    let goal = zuno_goal::GoalStore::from_pool(
        Arc::clone(&pool),
        root.path().join("goal-objective-spill"),
    )
    .expect("open ACP goal store")
    .goal(&session_id)
    .expect("read ACP goal")
    .expect("persisted ACP goal");
    let plan = zuno_tools::WorkStateStore::new(Arc::clone(&pool))
        .plan(&session_id)
        .expect("read ACP plan");
    assert!(
        plan.is_none(),
        "native Goal edits must not manufacture a user-visible generic Plan"
    );
    let connection = pool.get().expect("open ACP Plan archive");
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM work_plan_archive \
                 WHERE session_id=?1 AND state='superseded'",
                [&session_id],
                |row| row.get::<_, i64>(0),
            )
            .expect("count superseded ACP plans"),
        0,
        "native Goal edits must not manufacture or archive generic Plans"
    );
    assert_eq!(goal.objective, "What is GOAL-TWO?");
    assert_eq!(goal.status, zuno_goal::GoalStatus::Complete);

    let provider_requests = provider
        .received_requests()
        .await
        .expect("provider requests");
    let goal_turns = provider_requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| {
            body.get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| !tools.is_empty())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        goal_turns.len(),
        4,
        "each native Goal objective must drive one tool turn and one completion turn"
    );
    assert!(
        goal_turns
            .iter()
            .all(|body| !body.to_string().contains("/goal")),
        "native slash text must not enter provider input: {goal_turns:#?}"
    );
    assert!(
        goal_turns
            .iter()
            .any(|body| body.to_string().contains("What is GOAL-TWO?")),
        "the durable Goal objective was not injected into the autonomous provider turn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_load_recovers_an_active_goal_without_a_prior_user_message() {
    let provider = MockServer::start().await;
    let responder = GoalCompletionTurnResponder::default();
    Mock::given(method("POST"))
        .respond_with(responder.clone())
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("temporary ACP root");
    let config = danger_full_access_config(&provider.uri());

    let mut first = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start first zuno acp");
    let mut first_stdin = first.stdin.take().expect("first ACP stdin");
    let mut first_stdout = BufReader::new(first.stdout.take().expect("first ACP stdout"));
    request(
        &mut first_stdin,
        &mut first_stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut first_stdin,
        &mut first_stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let _commands = read_session_update(&mut first_stdout);
    request(
        &mut first_stdin,
        &mut first_stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    drop(first_stdin);
    assert!(first.wait().expect("wait for first ACP process").success());

    let location = zuno_paths::DbLocation::File(root.path().join("zuno-acp.db"));
    let pool = Arc::new(zuno_db::Pool::open(&location).expect("open ACP recovery store"));
    let goals = zuno_goal::GoalStore::from_pool(
        Arc::clone(&pool),
        root.path().join("goal-objective-spill"),
    )
    .expect("open ACP Goal store");
    let objective = "What is RECOVERED-GOAL?";
    goals
        .create_goal(&session_id, objective, None)
        .expect("seed active Goal without a user turn");
    let connection = pool.get().expect("open ACP message store");
    assert!(
        !zuno_db::message::MessageStore::new(&connection)
            .has_user_message_for_session(&session_id)
            .expect("inspect pre-recovery messages")
    );
    drop(connection);

    let mut second = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start second zuno acp");
    let mut second_stdin = second.stdin.take().expect("second ACP stdin");
    let mut second_stdout = BufReader::new(second.stdout.take().expect("second ACP stdout"));
    request(
        &mut second_stdin,
        &mut second_stdout,
        4,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let loaded = request(
        &mut second_stdin,
        &mut second_stdout,
        5,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert_eq!(loaded["modes"]["currentModeId"], "build");

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let goal = goals
                .goal(&session_id)
                .expect("read recovered Goal")
                .expect("persisted recovered Goal");
            if goal.status == zuno_goal::GoalStatus::Complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("active Goal did not resume after session/load");

    request(
        &mut second_stdin,
        &mut second_stdout,
        6,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    drop(second_stdin);
    assert!(
        second
            .wait()
            .expect("wait for second ACP process")
            .success()
    );

    let connection = pool.get().expect("open recovered ACP messages");
    let history = zuno_db::message::MessageStore::new(&connection)
        .hydrate_session(&session_id)
        .expect("hydrate recovered ACP session");
    let user_messages = history
        .iter()
        .filter(|message| message.info.role == zuno_db::message::MessageRole::User)
        .collect::<Vec<_>>();
    assert_eq!(
        user_messages.len(),
        1,
        "Goal recovery must create exactly one durable user anchor"
    );
    assert!(
        user_messages[0]
            .parts
            .iter()
            .any(|part| { part.data.get("text").and_then(Value::as_str) == Some(objective) })
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM session_input \
                 WHERE session_id=?1 AND state='consumed'",
                [&session_id],
                |row| row.get::<_, i64>(0),
            )
            .expect("count consumed Goal anchors"),
        1
    );
    drop(connection);

    let provider_requests = provider
        .received_requests()
        .await
        .expect("provider requests");
    let goal_turns = provider_requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<Value>(&request.body).ok())
        .filter(|body| {
            body.get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| !tools.is_empty())
        })
        .collect::<Vec<_>>();
    assert_eq!(responder.tool_requests.load(Ordering::SeqCst), 2);
    assert!(
        goal_turns
            .iter()
            .all(|body| !body.to_string().contains("/goal"))
    );
    assert!(
        goal_turns
            .iter()
            .any(|body| body.to_string().contains(objective)),
        "recovered Goal objective must anchor provider input: {goal_turns:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_load_rebuilds_resources_and_replays_for_each_load_request() {
    let mcp = MockServer::start().await;
    mount_remote_mcp_fixture(&mcp).await;
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("cold-check.md"),
        "Inspect the restored ACP session for $ARGUMENTS.",
    )
    .expect("write project command");
    let config = config_with_remote_mcp(&format!("{}/mcp", mcp.uri()), &provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    seed_durable_text_replay(root.path(), &session_id);

    let (_loaded, first_replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let cold_commands = read_session_update(&mut stdout);
    assert_eq!(cold_commands["sessionId"], session_id);
    assert_eq!(
        cold_commands["update"]["sessionUpdate"],
        "available_commands_update"
    );
    assert!(
        cold_commands["update"]["availableCommands"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command["name"] == "cold-check")),
        "a dormant load must publish project commands without activating MCP: {cold_commands}"
    );
    let initialize_after_load = mcp
        .received_requests()
        .await
        .expect("remote MCP requests")
        .into_iter()
        .filter(is_mcp_initialize)
        .count();

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/set_mode",
        json!({"sessionId": &session_id, "modeId": "plan"}),
    );
    request(
        &mut stdin,
        &mut stdout,
        6,
        "session/set_mode",
        json!({"sessionId": &session_id, "modeId": "build"}),
    );
    let initialize_after_reconfiguration = mcp
        .received_requests()
        .await
        .expect("remote MCP requests after reconfiguration")
        .into_iter()
        .filter(is_mcp_initialize)
        .count();

    let (_loaded_again, second_replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        7,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let (prompted, prompt_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        8,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"Activate the loaded ACP session."}]
        }),
    );
    let initialize_after_prompt = mcp
        .received_requests()
        .await
        .expect("remote MCP requests after prompt")
        .into_iter()
        .filter(is_mcp_initialize)
        .count();
    request(
        &mut stdin,
        &mut stdout,
        9,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP cold-load process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP cold-load process failed: {stderr}");
    }

    assert!(
        !first_replay.is_empty(),
        "the first session/load must reconstruct durable history"
    );
    assert!(
        !second_replay.is_empty(),
        "each explicit session/load reconstructs durable history for the requesting client"
    );
    assert_eq!(
        initialize_after_load, 2,
        "session/load must reconnect configured MCP before publishing the replacement session"
    );
    assert_eq!(
        initialize_after_reconfiguration, 4,
        "each transactional profile replacement must rebuild configured MCP resources"
    );
    assert_eq!(prompted["stopReason"], "end_turn");
    assert!(
        prompt_updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "ACP reply"
        }),
        "the first prompt after a cold load did not activate and drive the session"
    );
    assert_eq!(
        initialize_after_prompt, 5,
        "the second load already activated its replacement MCP resources before the prompt"
    );
}

fn is_mcp_initialize(request: &Request) -> bool {
    request.method.as_str() == "POST"
        && serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body["method"].as_str().map(str::to_owned))
            .as_deref()
            == Some("initialize")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_session_stdio_mcp_is_published_atomically_and_closed_exactly() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP stdio MCP root");
    let script = write_acp_stdio_mcp_server(root.path());
    let log = root.path().join("stdio-mcp.log");
    let declaration = acp_stdio_mcp_declaration("fixture", &script, &log, false);
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": [declaration]}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let started = wait_for_occurrences(&log, "start ", 1);
    assert!(started.contains(&format!("cwd={}", root.path().display())));
    assert!(started.contains("secret-ok"));
    let pid = started_pids(&started)[0];

    let (completed, _updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type":"text","text":"Use the fixture MCP tool if useful."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    let provider_requests = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(
        provider_requests
            .iter()
            .any(|request| { String::from_utf8_lossy(&request.body).contains("fixture_ping") }),
        "the provider request did not receive the session-local stdio MCP schema"
    );

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": session_id}),
    );
    wait_for_process_exit(pid);
    assert_eq!(
        std::fs::read_to_string(&log)
            .expect("MCP lifecycle log")
            .matches("start ")
            .count(),
        1
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP stdio MCP process");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("ACP stderr")
        .read_to_string(&mut stderr)
        .expect("read ACP stderr");
    assert!(status.success(), "ACP stdio MCP process failed: {stderr}");
    assert!(!stderr.contains(ACP_MCP_SENTINEL));
    assert_file_tree_excludes(root.path(), ACP_MCP_SENTINEL);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_session_http_mcp_uses_streamable_http_headers_and_disposes() {
    let mcp = MockServer::start().await;
    mount_remote_mcp_fixture(&mcp).await;
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP HTTP MCP root");
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({
            "cwd": root.path(),
            "mcpServers": [{
                "type": "http",
                "name": "web",
                "url": format!("{}/mcp", mcp.uri()),
                "headers": [{
                    "name": "Authorization",
                    "value": format!("Bearer {ACP_MCP_SENTINEL}")
                }]
            }]
        }),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let (completed, _updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type":"text","text":"Use the HTTP MCP tool if useful."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        provider
            .received_requests()
            .await
            .expect("provider requests")
            .iter()
            .any(|request| String::from_utf8_lossy(&request.body).contains("web_ping")),
        "the provider request did not receive the session-local HTTP MCP schema"
    );

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": session_id}),
    );
    let mcp_requests = mcp.received_requests().await.expect("MCP requests");
    assert!(mcp_requests.iter().any(|request| {
        request
            .headers
            .get("authorization")
            .is_some_and(|value| value.to_str().ok() == Some(&format!("Bearer {ACP_MCP_SENTINEL}")))
    }));
    assert_eq!(
        mcp_requests
            .iter()
            .filter(|request| request.method.as_str() == "DELETE")
            .count(),
        1,
        "closing the ACP session must close the remote MCP session exactly once"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP HTTP MCP process");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("ACP stderr")
        .read_to_string(&mut stderr)
        .expect("read ACP stderr");
    assert!(status.success(), "ACP HTTP MCP process failed: {stderr}");
    assert!(!stderr.contains(ACP_MCP_SENTINEL));
    assert_file_tree_excludes(root.path(), ACP_MCP_SENTINEL);
}

#[cfg(unix)]
#[test]
fn acp_session_mcp_partial_startup_rolls_back_before_publication() {
    let root = tempfile::tempdir().expect("ACP MCP rollback root");
    let script = write_acp_stdio_mcp_server(root.path());
    let good_log = root.path().join("good-mcp.log");
    let bad_log = root.path().join("bad-mcp.log");
    let config = config_with_second_model("https://example.invalid");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let error = request_failure(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({
            "cwd": root.path(),
            "mcpServers": [
                acp_stdio_mcp_declaration("good", &script, &good_log, false),
                acp_stdio_mcp_declaration("bad", &script, &bad_log, true)
            ]
        }),
    );
    assert!(error["message"].as_str().is_some_and(|message| {
        message.contains("ACP MCP server `bad` failed")
            && !message.contains(ACP_MCP_SENTINEL)
            && !message.contains(script.to_string_lossy().as_ref())
    }));
    let good = wait_for_occurrences(&good_log, "start ", 1);
    let bad = wait_for_occurrences(&bad_log, "start ", 1);
    wait_for_process_exit(started_pids(&good)[0]);
    wait_for_process_exit(started_pids(&bad)[0]);
    assert_eq!(good.matches("start ").count(), 1);
    assert_eq!(bad.matches("start ").count(), 1);

    let created = request(
        &mut stdin,
        &mut stdout,
        3,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": session_id}),
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP MCP rollback process");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("ACP stderr")
        .read_to_string(&mut stderr)
        .expect("read ACP stderr");
    assert!(
        status.success(),
        "ACP MCP rollback process failed: {stderr}"
    );
    assert!(!stderr.contains(ACP_MCP_SENTINEL));
    assert_file_tree_excludes(root.path(), ACP_MCP_SENTINEL);
}

#[cfg(unix)]
#[test]
fn acp_load_and_resume_recreate_only_the_mcp_list_supplied_by_the_client() {
    let root = tempfile::tempdir().expect("ACP MCP resume root");
    let script = write_acp_stdio_mcp_server(root.path());
    let log = root.path().join("resume-mcp.log");
    let declaration = acp_stdio_mcp_declaration("fixture", &script, &log, false);
    let config = config_with_second_model("https://example.invalid");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": [declaration.clone()]}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let first = wait_for_occurrences(&log, "start ", 1);
    let first_pid = started_pids(&first)[0];
    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    wait_for_process_exit(first_pid);

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/resume",
        json!({"sessionId": &session_id, "cwd": root.path(), "mcpServers": []}),
    );
    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    assert_eq!(
        wait_for_occurrences(&log, "start ", 1)
            .matches("start ")
            .count(),
        1,
        "an empty resume list must not reuse the prior process"
    );

    request(
        &mut stdin,
        &mut stdout,
        6,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": [declaration.clone()]
        }),
    );
    let second = wait_for_occurrences(&log, "start ", 2);
    let second_pid = *started_pids(&second).last().expect("second MCP pid");
    request(
        &mut stdin,
        &mut stdout,
        7,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    wait_for_process_exit(second_pid);

    request(
        &mut stdin,
        &mut stdout,
        8,
        "session/resume",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": [declaration]
        }),
    );
    let third = wait_for_occurrences(&log, "start ", 3);
    let third_pid = *started_pids(&third).last().expect("third MCP pid");
    request(
        &mut stdin,
        &mut stdout,
        9,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    wait_for_process_exit(third_pid);
    let lifecycle = std::fs::read_to_string(&log).expect("MCP lifecycle log");
    assert_eq!(
        lifecycle.matches("start ").count(),
        3,
        "unexpected MCP starts:\n{lifecycle}"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP MCP resume process");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("ACP stderr")
        .read_to_string(&mut stderr)
        .expect("read ACP stderr");
    assert!(status.success(), "ACP MCP resume process failed: {stderr}");
    assert!(!stderr.contains(ACP_MCP_SENTINEL));
    assert_file_tree_excludes(root.path(), ACP_MCP_SENTINEL);
}

#[test]
fn acp_connection_bounds_open_sessions_and_releases_capacity_on_close() {
    let root = tempfile::tempdir().expect("ACP test root");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let mut sessions = Vec::new();
    for index in 0_u64..32 {
        let created = request(
            &mut stdin,
            &mut stdout,
            index + 2,
            "session/new",
            json!({"cwd": root.path(), "mcpServers": []}),
        );
        sessions.push(
            created["sessionId"]
                .as_str()
                .expect("session id")
                .to_owned(),
        );
    }

    let capacity = request_failure(
        &mut stdin,
        &mut stdout,
        34,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    assert_eq!(capacity["code"], -32602);
    assert!(
        capacity["message"]
            .as_str()
            .is_some_and(|message| message.contains("32 open sessions")),
        "capacity failure must explain how to recover: {capacity}"
    );

    request(
        &mut stdin,
        &mut stdout,
        35,
        "session/close",
        json!({"sessionId": &sessions[0]}),
    );
    let replacement = request(
        &mut stdin,
        &mut stdout,
        36,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    assert!(replacement["sessionId"].as_str().is_some());

    drop(stdin);
    let status = child.wait().expect("wait for ACP capacity process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP capacity process failed: {stderr}");
    }
}

#[test]
fn acp_reasoning_levels_follow_the_active_models_declared_variants() {
    let root = tempfile::tempdir().expect("ACP test root");
    let config = reasoning_config();
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("new session id")
        .to_owned();
    let reasoning = created["configOptions"]
        .as_array()
        .and_then(|options| {
            options
                .iter()
                .find(|option| option["id"] == "reasoning_effort")
        })
        .expect("reasoning selector");
    assert_eq!(reasoning["category"], "thought_level");
    assert_eq!(
        reasoning["options"]
            .as_array()
            .expect("reasoning options")
            .iter()
            .filter_map(|option| option["value"].as_str())
            .collect::<Vec<_>>(),
        vec!["default", "low", "xhigh", "max"]
    );

    let selected = request(
        &mut stdin,
        &mut stdout,
        3,
        "session/set_config_option",
        json!({
            "sessionId": &session_id,
            "configId": "reasoning_effort",
            "value": "xhigh"
        }),
    );
    assert!(selected["configOptions"].as_array().is_some_and(|options| {
        options
            .iter()
            .any(|option| option["id"] == "reasoning_effort" && option["currentValue"] == "xhigh")
    }));

    let plain = request(
        &mut stdin,
        &mut stdout,
        4,
        "session/set_config_option",
        json!({
            "sessionId": &session_id,
            "configId": "model",
            "value": "test/test-model-2"
        }),
    );
    assert!(
        plain["configOptions"].as_array().is_some_and(|options| {
            options
                .iter()
                .all(|option| option["id"] != "reasoning_effort")
        }),
        "a non-reasoning model must remove the stale thought-level selector: {plain}"
    );

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/close",
        json!({"sessionId": session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP reasoning process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP reasoning process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_prompt_drives_the_durable_turn_and_streams_updates() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let command_dir = root.path().join(".zuno/command");
    std::fs::create_dir_all(&command_dir).expect("create project command directory");
    std::fs::write(
        command_dir.join("acp-check.md"),
        "Inspect the ACP integration for $ARGUMENTS.",
    )
    .expect("write project command");
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let configured = request(
        &mut stdin,
        &mut stdout,
        3,
        "session/set_config_option",
        json!({
            "sessionId": session_id,
            "configId": "model",
            "value": "test/test-model-2"
        }),
    );
    assert!(
        configured["configOptions"]
            .as_array()
            .is_some_and(|options| {
                options.iter().any(|option| {
                    option["id"] == "model" && option["currentValue"] == "test/test-model-2"
                })
            }),
        "model selection did not rebuild the ACP session: {configured}"
    );
    let (command_completed, command_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "/acp-check src/lib.rs"}]
        }),
    );
    assert_eq!(command_completed["stopReason"], "end_turn");
    assert!(
        command_updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "ACP reply"
        }),
        "configured slash command did not execute a real turn: {command_updates:?}"
    );
    let resource_path = root.path().join("notes.md");
    std::fs::write(&resource_path, "# Design notes\n").expect("write ACP resource link target");
    let resource_uri = url::Url::from_file_path(&resource_path)
        .expect("ACP resource path must convert to a file URI")
        .to_string();
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        5,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [
                {"type": "text", "text": "Answer the ACP prompt."},
                {
                    "type": "resource_link",
                    "name": "notes.md",
                    "title": "Design notes",
                    "description": "ACP design context",
                    "mimeType": "text/markdown",
                    "size": 42,
                    "uri": resource_uri
                }
            ]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "ACP reply"
        }),
        "missing streamed assistant text: {updates:?}"
    );
    assert!(
        updates.iter().any(|update| {
            update["sessionUpdate"] == "usage_update"
                && update["used"] == 11
                && update["size"] == 120_000
        }),
        "missing ACP usage projection: {updates:?}"
    );

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(
        received
            .iter()
            .any(|request| { String::from_utf8_lossy(&request.body).contains(&resource_uri) }),
        "resource link was not preserved in the provider request"
    );
    assert!(
        received.iter().any(|request| {
            String::from_utf8_lossy(&request.body)
                .contains("Inspect the ACP integration for src/lib.rs.")
        }),
        "configured slash command was not expanded through the shared command driver"
    );
    assert!(
        received.iter().any(|request| {
            let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
            body["model"] == "test-model-2"
        }),
        "the selected ACP model was not used for the provider request: {received:?}"
    );

    request(
        &mut stdin,
        &mut stdout,
        6,
        "session/close",
        json!({"sessionId": session_id}),
    );
    let (_loaded, replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        7,
        "session/load",
        json!({
            "sessionId": session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    let resource = replay
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["type"] == "resource_link"
        })
        .expect("session/load did not replay the typed resource link");
    assert_eq!(resource["content"]["name"], "notes.md");
    assert_eq!(resource["content"]["title"], "Design notes");
    assert_eq!(resource["content"]["description"], "ACP design context");
    assert_eq!(resource["content"]["mimeType"], "text/markdown");
    assert_eq!(resource["content"]["size"], 42);
    assert_eq!(resource["content"]["uri"], resource_uri);

    drop(stdin);
    let status = child.wait().expect("wait for ACP prompt process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP prompt process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_images_and_embedded_context_reach_the_provider_and_durable_replay() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(TextTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let config = rich_prompt_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let selection_uri = "zed://selection/src/lib.rs#L1-L3";
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [
                {
                    "type": "image",
                    "mimeType": "image/png",
                    "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DAAAAEAQEARwbK3gAAAABJRU5ErkJggg==",
                    "uri": "file:///tmp/screenshot.png"
                },
                {
                    "type": "resource",
                    "resource": {
                        "uri": selection_uri,
                        "mimeType": "text/rust",
                        "text": "fn selected() {}"
                    }
                },
                {"type": "text", "text": "Review this context."}
            ]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk" && update["content"]["text"] == "ACP reply"
    }));

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(received.iter().any(|request| {
        let body = String::from_utf8_lossy(&request.body);
        body.contains("data:image/png;base64,")
            && body.contains(selection_uri)
            && body.contains("fn selected() {}")
    }));

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    let (_loaded, replay) = request_with_updates(
        &mut stdin,
        &mut stdout,
        5,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert!(replay.iter().any(|update| {
        update["sessionUpdate"] == "user_message_chunk"
            && update["content"]["type"] == "image"
            && update["content"]["mimeType"] == "image/png"
    }));
    assert!(replay.iter().any(|update| {
        update["sessionUpdate"] == "user_message_chunk"
            && update["content"]["type"] == "text"
            && update["content"]["text"].as_str().is_some_and(|text| {
                text.contains(selection_uri) && text.contains("fn selected() {}")
            })
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP rich prompt process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP rich prompt process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_plan_round_trips_question_tool_through_stable_elicitation() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(QuestionTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "elicitation": {
                    "form": {}
                }
            }
        }),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/set_mode",
        json!({"sessionId": &session_id, "modeId": "plan"}),
    );
    let (completed, updates, elicitation) = request_with_elicitation(
        &mut stdin,
        &mut stdout,
        4,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type": "text", "text": "Which database should I choose?"}]
        }),
        &session_id,
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert_eq!(elicitation["toolCallId"], "call_question");
    let continuing_index = updates
        .iter()
        .position(|update| {
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == "call_question"
                && update["status"] == "in_progress"
                && update["_meta"]["zuno"]["question"]["status"] == "answered"
                && update["_meta"]["zuno"]["question"]["continuationPending"] == true
        })
        .unwrap_or_else(|| panic!("missing post-answer continuation update: {updates:?}"));
    let settled_index = updates
        .iter()
        .position(|update| {
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == "call_question"
                && update["status"] == "completed"
                && update["_meta"]["zuno"]["question"]["status"] == "answered"
                && update["_meta"]["zuno"]["question"]["continuationPending"] == false
        })
        .unwrap_or_else(|| panic!("missing checkpointed question completion: {updates:?}"));
    let reply_index = updates
        .iter()
        .position(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "Configured SQLite"
        })
        .unwrap_or_else(|| {
            panic!("missing streamed assistant text after elicitation: {updates:?}")
        });
    assert!(
        continuing_index < settled_index && settled_index < reply_index,
        "question activity must remain live until the continuation checkpoint: {updates:?}"
    );

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(
        received.iter().any(|request| {
            let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "tool"
                        && message["tool_call_id"] == "call_question"
                        && message["content"].as_str().is_some_and(|content| {
                            content.contains(r#""Which database?"="SQLite""#)
                        })
                })
            })
        }),
        "provider never received the accepted question answer: {received:?}"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP HITL process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP HITL process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_prompt_streams_a_negotiated_foreground_subagent_on_its_child_session() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(SubagentTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(&provider.uri())).expect("test config");
    config["permission"] = json!({"mode":"allow_all"});
    config["sandbox"] = json!({"mode":"danger-full-access"});
    let config = serde_json::to_string(&config).expect("encode subagent config");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "subagents": {}
            }
        }),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let parent_session_id = created["sessionId"]
        .as_str()
        .expect("parent session id")
        .to_owned();
    let (completed, routed) = request_with_routed_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &parent_session_id,
            "prompt": [{"type":"text","text":"Delegate the inspection."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");

    let spawned = routed
        .iter()
        .find(|params| params["update"]["sessionUpdate"] == "subagent_spawned")
        .expect("negotiated child spawn");
    assert_eq!(spawned["sessionId"], parent_session_id);
    assert_eq!(spawned["update"]["name"], "explorer");
    assert!(
        spawned["update"]["task"]
            .as_str()
            .is_some_and(|task| task.contains("Instructions:\nInspect child live stream.")),
        "negotiated child spawn did not carry the rendered delegation contract: {spawned:#?}"
    );
    let child_session_id = spawned["update"]["subagentSessionId"]
        .as_str()
        .expect("child session id");
    assert!(routed.iter().any(|params| {
        params["sessionId"] == child_session_id
            && params["update"]["sessionUpdate"] == "user_message_chunk"
            && params["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Instructions:\nInspect child live stream."))
    }));
    assert!(routed.iter().any(|params| {
        params["sessionId"] == child_session_id
            && params["update"]["sessionUpdate"] == "agent_message_chunk"
            && params["update"]["content"]["text"] == "child live result"
    }));
    let terminal_index = routed
        .iter()
        .position(|params| {
            params["sessionId"] == parent_session_id
                && params["update"]["sessionUpdate"] == "subagent_state_update"
                && params["update"]["subagentSessionId"] == child_session_id
                && params["update"]["state"] == "completed"
        })
        .expect("child terminal state");
    let child_result_index = routed
        .iter()
        .position(|params| {
            params["sessionId"] == child_session_id
                && params["update"]["sessionUpdate"] == "agent_message_chunk"
                && params["update"]["content"]["text"] == "child live result"
        })
        .expect("child result");
    assert!(
        child_result_index < terminal_index,
        "the terminal state must follow all child output: {routed:#?}"
    );
    assert!(
        routed.iter().any(|params| {
            params["sessionId"] == parent_session_id
                && params["update"]["sessionUpdate"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("tool_call"))
                && params["update"]["_meta"]["zuno"]["subagent"]["agent"] == "explorer"
        }),
        "the stable task fallback must remain beside the negotiated child stream: {routed:#?}"
    );

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": &parent_session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_fallback_reports_child_blocker_without_synchronous_elicitation() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ParentReportsChildBlockerTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(&provider.uri())).expect("test config");
    config["permission"] = json!({"mode":"allow_all"});
    config["sandbox"] = json!({"mode":"danger-full-access"});
    let config = serde_json::to_string(&config).expect("encode child question config");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "elicitation": {
                    "form": {}
                }
            }
        }),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let parent_session_id = created["sessionId"]
        .as_str()
        .expect("parent session id")
        .to_owned();
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &parent_session_id,
            "prompt": [{"type":"text","text":"Can a child choose the database?"}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        updates.iter().all(|update| {
            update["sessionUpdate"] != "tool_call" || update["toolCallId"] != "call_question"
        }),
        "ordinary work must report the blocker directly instead of parking on elicitation: \
         {updates:#?}"
    );
    assert!(
        updates
            .iter()
            .all(|update| update["sessionUpdate"] != "subagent_spawned"),
        "unnegotiated clients must receive only the stable task fallback: {updates:#?}"
    );
    assert!(updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"] == "Which database should be used: Postgres or SQLite?"
    }));

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": &parent_session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_fallback_routes_child_permission_through_the_declared_root_session() {
    let provider = MockServer::start().await;
    let root = tempfile::tempdir().expect("ACP test root");
    let target = root.path().join("created-by-child.txt");
    Mock::given(method("POST"))
        .respond_with(SubagentWriteTurnResponder {
            path: target.to_string_lossy().into_owned(),
        })
        .mount(&provider)
        .await;
    let config = strict_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let parent_session_id = created["sessionId"]
        .as_str()
        .expect("parent session id")
        .to_owned();
    let (completed, updates, permissions) = request_with_permissions(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &parent_session_id,
            "prompt": [{"type":"text","text":"Delegate the file creation."}]
        }),
    );

    assert_eq!(completed["stopReason"], "end_turn");
    let permission = permissions
        .iter()
        .find(|request| request["toolCall"]["toolCallId"] == "call_child_write")
        .expect("child write permission");
    assert_eq!(permission["sessionId"], parent_session_id);
    let durable_child_id = permission["_meta"]["zuno"]["childSessionId"]
        .as_str()
        .expect("durable child id in fallback permission metadata");
    assert_ne!(durable_child_id, parent_session_id);
    assert_eq!(permission["toolCall"]["kind"], "edit");
    assert!(
        updates
            .iter()
            .all(|update| update["sessionUpdate"] != "subagent_spawned"),
        "unnegotiated clients must keep the stable task fallback: {updates:#?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("delegated file"),
        "created by ACP child\n"
    );

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": &parent_session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_close_cancels_and_joins_its_background_child_without_native_projection() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(BackgroundSubagentTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(&provider.uri())).expect("test config");
    config["permission"] = json!({"mode":"allow_all"});
    config["sandbox"] = json!({"mode":"danger-full-access"});
    let config = serde_json::to_string(&config).expect("encode background child config");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "subagents": {}
            }
        }),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let parent_session_id = created["sessionId"]
        .as_str()
        .expect("parent session id")
        .to_owned();
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &parent_session_id,
            "prompt": [{"type":"text","text":"Start a background child."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        updates
            .iter()
            .all(|update| update["sessionUpdate"] != "subagent_spawned"),
        "background jobs remain on the stable task/job lifecycle: {updates:#?}"
    );
    assert!(updates.iter().any(|update| {
        update["_meta"]["zuno"]["subagent"]["background"] == true
            && update["_meta"]["zuno"]["subagent"]["state"] == "running"
    }));

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let child_request_seen = provider
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .any(|request| {
                    serde_json::from_slice::<Value>(&request.body)
                        .ok()
                        .and_then(|body| body.get("messages").cloned())
                        .is_some_and(|messages| {
                            messages
                                .to_string()
                                .contains("Wait until the parent session closes.")
                        })
                });
            if child_request_seen {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background child reaches the provider before close");

    let (closed_tx, closed_rx) = mpsc::sync_channel(1);
    let close_session_id = parent_session_id.clone();
    std::thread::spawn(move || {
        let response = request(
            &mut stdin,
            &mut stdout,
            4,
            "session/close",
            json!({"sessionId": close_session_id}),
        );
        let _sent = closed_tx.send(response);
    });
    let closed = match closed_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(closed) => closed,
        Err(error) => {
            let _killed = child.kill();
            panic!("session/close did not join the cancelled background child: {error}");
        }
    };
    assert_eq!(closed, json!({}));
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }

    let location = zuno_paths::DbLocation::File(root.path().join("zuno-acp.db"));
    let pool = Arc::new(zuno_db::Pool::open(&location).expect("open ACP background-job store"));
    let jobs = zuno_db::job::AgentJobStore::new(pool)
        .list_for_parent(&parent_session_id)
        .expect("list ACP background jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, zuno_db::job::JobStatus::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_detached_parent_projects_final_plan_after_background_child_completion() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(CompletingBackgroundSubagentTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let mut config: Value =
        serde_json::from_str(&config_with_second_model(&provider.uri())).expect("test config");
    config["permission"] = json!({"mode":"allow_all"});
    config["sandbox"] = json!({"mode":"danger-full-access"});
    let config = serde_json::to_string(&config).expect("encode background child config");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "subagents": {}
            }
        }),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let parent_session_id = created["sessionId"]
        .as_str()
        .expect("parent session id")
        .to_owned();
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &parent_session_id,
            "prompt": [{"type":"text","text":"Start work and wait for its background result."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        updates.iter().any(|update| {
            update["_meta"]["zuno"]["subagent"]["background"] == true
                && update["_meta"]["zuno"]["subagent"]["state"] == "running"
        }),
        "the request-owned turn did not publish the running background job: {updates:#?}"
    );
    assert!(updates.iter().all(|update| {
        update["sessionUpdate"] != "agent_message_chunk"
            || update["content"]["text"].as_str().is_none_or(|text| {
                !text.contains("parent resumed after background child completion")
            })
    }));

    let (resumed_tx, resumed_rx) = mpsc::sync_channel(1);
    let resumed_session_id = parent_session_id.clone();
    std::thread::spawn(move || {
        let mut detached_updates = Vec::new();
        let mut resumed = false;
        let mut projected_plan = false;
        loop {
            let params = read_session_update(&mut stdout);
            resumed |= params["sessionId"] == resumed_session_id
                && params["update"]["sessionUpdate"] == "agent_message_chunk"
                && params["update"]["content"]["text"]
                    .as_str()
                    .is_some_and(|text| {
                        text.contains("parent resumed after background child completion")
                    });
            projected_plan |= params["sessionId"] == resumed_session_id
                && params["update"]["sessionUpdate"] == "plan";
            detached_updates.push(params);
            if resumed && projected_plan {
                let _sent = resumed_tx.send((stdout, detached_updates));
                break;
            }
        }
    });
    let (mut stdout, detached_updates) = match resumed_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(result) => result,
        Err(error) => {
            let _killed = child.kill();
            panic!(
                "background child did not resume the completed parent turn and project its final \
                 plan: {error}"
            );
        }
    };
    assert!(detached_updates.iter().any(|params| {
        params["sessionId"] == parent_session_id
            && params["update"]["sessionUpdate"] == "agent_message_chunk"
            && params["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| {
                    text.contains("parent resumed after background child completion")
                })
    }));
    let resumed_index = detached_updates
        .iter()
        .position(|params| {
            params["sessionId"] == parent_session_id
                && params["update"]["sessionUpdate"] == "agent_message_chunk"
                && params["update"]["content"]["text"]
                    .as_str()
                    .is_some_and(|text| {
                        text.contains("parent resumed after background child completion")
                    })
        })
        .expect("missing resumed parent message");
    let (plan_index, plan_update) = detached_updates
        .iter()
        .enumerate()
        .rev()
        .find(|(_, params)| {
            params["sessionId"] == parent_session_id && params["update"]["sessionUpdate"] == "plan"
        })
        .expect("missing final durable plan projection");
    assert!(
        plan_index > resumed_index,
        "the final durable plan projection must follow detached turn events"
    );
    assert!(
        plan_update["update"]["entries"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()),
        "the final durable plan projection must contain entries"
    );
    let projected_revision = plan_update["update"]["_meta"]["zuno"]["revision"]
        .as_i64()
        .expect("the final durable plan projection must carry its durable revision");
    let location = zuno_paths::DbLocation::File(root.path().join("zuno-acp.db"));
    let pool = Arc::new(zuno_db::Pool::open(&location).expect("open ACP work-state pool"));
    let durable_plan = zuno_tools::WorkStateStore::new(Arc::clone(&pool))
        .plan(&parent_session_id)
        .expect("read final durable plan")
        .expect("background continuation must retain its plan");
    assert_eq!(
        projected_revision, durable_plan.revision,
        "ACP must project the final durable plan revision"
    );
    assert!(
        zuno_db::human_request::HumanRequestStore::new(pool)
            .pending(Some(&parent_session_id))
            .expect("read pending reconciliation requests")
            .is_empty(),
        "authoritative background completion must cancel the stale reconciliation request"
    );

    request(
        &mut stdin,
        &mut stdout,
        4,
        "session/close",
        json!({"sessionId": &parent_session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_prompt_projects_the_final_durable_plan_before_responding() {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(PlanTurnResponder)
        .mount(&provider)
        .await;
    let root = tempfile::tempdir().expect("ACP test root");
    let config = config_with_second_model(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let (completed, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type":"text","text":"Create a durable plan."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        updates.iter().any(|update| {
            update["toolCallId"] == "call_plan"
                && update["rawInput"]["title"] == "Verify ACP projection"
        }),
        "plan tool input was not projected: {updates:#?}"
    );
    let plan_positions = updates
        .iter()
        .enumerate()
        .filter(|(_, update)| update["sessionUpdate"] == "plan")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert!(
        plan_positions.len() >= 2,
        "a durable Plan commit must be projected immediately and again as the terminal \
         authoritative snapshot: {updates:#?}"
    );
    let final_message = updates
        .iter()
        .rposition(|update| update["sessionUpdate"] == "agent_message_chunk")
        .expect("final assistant message projection");
    assert!(
        plan_positions[0] < final_message,
        "the first Plan update must follow the durable tool commit rather than waiting for the \
         terminal answer: {updates:#?}"
    );
    let plan = updates
        .iter()
        .rfind(|update| update["sessionUpdate"] == "plan")
        .expect("session/prompt returned without the final durable plan snapshot");
    assert_eq!(
        plan["entries"].as_array().map(Vec::len),
        Some(2),
        "final projected plan did not reflect the successful model refinement: {updates:#?}"
    );
    assert_eq!(plan["entries"][0]["content"], "Inspect ACP plan projection");
    assert_eq!(plan["entries"][0]["status"], "completed");
    assert_eq!(plan["entries"][1]["content"], "Verify Zed update");
    assert_eq!(plan["entries"][1]["status"], "completed");
    assert_eq!(plan["_meta"]["zuno"]["title"], "Verify ACP projection");

    let received = provider
        .received_requests()
        .await
        .expect("provider requests");
    assert!(received.iter().any(|request| {
        let body: Value = serde_json::from_slice(&request.body).expect("provider request JSON");
        body["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call_plan")
        })
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP plan process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP plan process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_write_round_trips_strict_permission_and_native_creation_diff() {
    let root = tempfile::tempdir().expect("ACP test root");
    let target = root.path().join("created.txt");
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(WriteTurnResponder {
            path: target.to_string_lossy().into_owned(),
        })
        .mount(&provider)
        .await;
    let config = strict_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let (completed, updates, permissions) = request_with_permissions(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": &session_id,
            "prompt": [{"type":"text","text":"Create the requested file."}]
        }),
    );
    assert_eq!(completed["stopReason"], "end_turn");
    assert_eq!(
        permissions.len(),
        1,
        "unexpected permission flow: {permissions:#?}"
    );
    let permission = &permissions[0];
    assert_eq!(permission["sessionId"], session_id);
    assert_eq!(permission["toolCall"]["toolCallId"], "call_write");
    assert_eq!(permission["toolCall"]["kind"], "edit");
    assert_eq!(permission["toolCall"]["rawInput"]["permission"], "edit");
    assert!(permission["options"].as_array().is_some_and(|options| {
        options
            .iter()
            .any(|option| option["optionId"] == "allow_once")
    }));
    assert_eq!(
        std::fs::read_to_string(&target).expect("created file"),
        "created through ACP\n"
    );
    assert!(updates.iter().any(|update| {
        update["toolCallId"] == "call_write"
            && update["rawInput"]["filePath"] == target.to_string_lossy().as_ref()
    }));
    let written = updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == "call_write"
                && update["status"] == "completed"
                && update["content"].is_array()
        })
        .expect("completed write projection");
    let diff = written["content"]
        .as_array()
        .expect("write content")
        .iter()
        .find(|content| content["type"] == "diff")
        .expect("native creation diff");
    let target_wire = zuno_paths::wire_path(
        &std::fs::canonicalize(&target).expect("created ACP target canonicalizes"),
    );
    assert_eq!(diff["path"], target_wire);
    assert_eq!(diff["oldText"], Value::Null);
    assert_eq!(diff["newText"], "created through ACP\n");
    assert!(written["locations"].as_array().is_some_and(|locations| {
        locations
            .iter()
            .any(|location| location["path"] == target_wire)
    }));
    assert!(updates.iter().any(|update| {
        update["sessionUpdate"] == "agent_message_chunk"
            && update["content"]["text"] == "File created"
    }));

    drop(stdin);
    let status = child.wait().expect("wait for ACP write process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP write process failed: {stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_danger_full_access_emits_no_tool_approval_request() {
    let root = tempfile::tempdir().expect("ACP test root");
    let target = root.path().join("created-without-approval.txt");
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(WriteTurnResponder {
            path: target.to_string_lossy().into_owned(),
        })
        .mount(&provider)
        .await;
    let config = danger_full_access_config(&provider.uri());
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"].as_str().expect("session id");
    let (completed, _updates, permissions) = request_with_permissions(
        &mut stdin,
        &mut stdout,
        3,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type":"text","text":"Create the requested file."}]
        }),
    );

    assert_eq!(completed["stopReason"], "end_turn");
    assert!(
        permissions.is_empty(),
        "danger-full-access must not emit ACP approval requests: {permissions:#?}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).expect("created file"),
        "created through ACP\n"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }
}

#[test]
fn acp_load_replays_durable_content_tools_plan_and_usage() {
    let root = tempfile::tempdir().expect("ACP test root");
    let mut child = isolated_command(root.path())
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({"protocolVersion": 1}),
    );
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let session_id = created["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    seed_durable_replay(root.path(), &session_id);

    let (loaded, updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert_eq!(loaded["modes"]["currentModeId"], "build");
    let kinds = updates
        .iter()
        .filter_map(|update| update["sessionUpdate"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            "user_message_chunk",
            "user_message_chunk",
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "plan",
            "session_info_update",
            "usage_update",
        ],
        "session/load must replay every durable client-visible projection in order: {updates:#?}"
    );
    assert_eq!(updates[0]["messageId"], "msg_acp_load_user");
    assert_eq!(updates[0]["content"]["text"], "replay this durable session");
    assert_eq!(updates[1]["content"]["type"], "image");
    assert_eq!(updates[1]["content"]["mimeType"], "image/png");
    assert_eq!(updates[2]["messageId"], "msg_acp_load_assistant");
    let completed = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call_update")
        .expect("completed tool replay");
    let content = completed["content"].as_array().expect("tool content");
    let diff = content
        .iter()
        .find(|item| item["type"] == "diff")
        .expect("typed ACP diff");
    assert_eq!(diff["oldText"], "old\n");
    assert_eq!(diff["newText"], "new\n");
    assert!(
        diff["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("/src/lib.rs")),
        "typed diff path must be absolute: {diff}"
    );
    assert!(content.iter().any(|item| {
        item["type"] == "content"
            && item["content"]["type"] == "image"
            && item["content"]["mimeType"] == "image/png"
    }));
    let plan = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "plan")
        .expect("plan replay");
    assert_eq!(plan["entries"][0]["status"], "in_progress");
    assert_eq!(plan["entries"][1]["status"], "pending");
    let learning = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "session_info_update")
        .expect("learning replay");
    assert_eq!(
        learning["_meta"]["zuno"]["learning"]["experiences"],
        json!([])
    );
    let usage = updates
        .iter()
        .find(|update| update["sessionUpdate"] == "usage_update")
        .expect("usage replay");
    assert_eq!(usage["used"], 175);
    assert_eq!(usage["size"], 100_000);
    assert_eq!(usage["cost"], json!({"amount":1.25,"currency":"USD"}));

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/close",
        json!({"sessionId": &session_id}),
    );
    let (resumed, resume_updates) = request_with_updates(
        &mut stdin,
        &mut stdout,
        6,
        "session/resume",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert_eq!(resumed["modes"]["currentModeId"], "build");
    assert!(
        resume_updates.is_empty(),
        "session/resume must not duplicate durable history: {resume_updates:#?}"
    );
    let (_loaded_after_resume, replay_after_resume) = request_with_updates(
        &mut stdin,
        &mut stdout,
        7,
        "session/load",
        json!({
            "sessionId": &session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );
    assert_eq!(
        replay_after_resume, updates,
        "session/load is an explicit request to reconstruct durable client-visible state even \
         after a non-replaying session/resume"
    );

    drop(stdin);
    let status = child.wait().expect("wait for ACP load process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP load process failed: {stderr}");
    }
}

#[test]
fn acp_load_replays_negotiated_child_sessions_on_their_own_routes() {
    let root = tempfile::tempdir().expect("ACP test root");
    let mut config: Value = serde_json::from_str(TEST_CONFIG).expect("test config JSON");
    config["sandbox"] = json!({"mode":"danger-full-access"});
    let config = serde_json::to_string(&config).expect("encode child replay config");
    let mut child = isolated_command_with_config(root.path(), &config)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(acp_stderr())
        .spawn()
        .expect("start zuno acp");
    let mut stdin = child.stdin.take().expect("ACP stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("ACP stdout"));

    let initialized = request(
        &mut stdin,
        &mut stdout,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "subagents": {}
            }
        }),
    );
    assert!(initialized["agentCapabilities"]["sessionCapabilities"]["subagents"].is_object());
    let created = request(
        &mut stdin,
        &mut stdout,
        2,
        "session/new",
        json!({"cwd": root.path(), "mcpServers": []}),
    );
    let parent_session_id = created["sessionId"]
        .as_str()
        .expect("parent session id")
        .to_owned();
    request(
        &mut stdin,
        &mut stdout,
        3,
        "session/close",
        json!({"sessionId": &parent_session_id}),
    );
    let child_session_id = "ses_acp_replayed_child";
    seed_durable_child_replay(root.path(), &parent_session_id, child_session_id);

    let (_loaded, routed) = request_with_routed_updates(
        &mut stdin,
        &mut stdout,
        4,
        "session/load",
        json!({
            "sessionId": &parent_session_id,
            "cwd": root.path(),
            "mcpServers": []
        }),
    );

    let routes = routed
        .iter()
        .map(|params| {
            (
                params["sessionId"].as_str().unwrap_or_default(),
                params["update"]["sessionUpdate"]
                    .as_str()
                    .unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        routes,
        vec![
            (parent_session_id.as_str(), "session_info_update"),
            (parent_session_id.as_str(), "subagent_spawned"),
            (child_session_id, "user_message_chunk"),
            (child_session_id, "agent_message_chunk"),
            (child_session_id, "session_info_update"),
            (child_session_id, "usage_update"),
            (parent_session_id.as_str(), "subagent_state_update"),
        ],
        "session/load must rebuild the durable child tree before returning: {routed:#?}"
    );
    assert_eq!(
        routed[0]["update"]["_meta"]["zuno"]["learning"]["experiences"],
        json!([])
    );
    assert_eq!(routed[1]["update"]["subagentSessionId"], child_session_id);
    assert_eq!(routed[1]["update"]["name"], "explorer");
    assert_eq!(
        routed[2]["update"]["content"]["text"],
        "inspect the child tree"
    );
    assert_eq!(
        routed[3]["update"]["content"]["text"],
        "child history restored"
    );
    assert_eq!(
        routed[4]["update"]["_meta"]["zuno"]["learning"]["experiences"],
        json!([])
    );
    assert_eq!(routed[6]["update"]["state"], "disconnected");

    request(
        &mut stdin,
        &mut stdout,
        5,
        "session/close",
        json!({"sessionId": &parent_session_id}),
    );
    drop(stdin);
    let status = child.wait().expect("wait for ACP process");
    if !status.success() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("ACP stderr")
            .read_to_string(&mut stderr)
            .expect("read ACP stderr");
        panic!("ACP process failed: {stderr}");
    }
}
