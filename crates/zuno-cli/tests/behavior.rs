use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn zuno() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zuno"))
}

fn isolated(command: &mut Command, root: &Path) {
    command
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true");
}

fn run(root: &Path, args: &[&str]) -> Output {
    let mut command = zuno();
    command.args(args).current_dir(root);
    isolated(&mut command, root);
    command.output().expect("run zuno CLI")
}

fn models_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../zuno-llm/tests/fixtures/models-dev-pinned.json")
}

fn configure_models(command: &mut Command) {
    command
        .env("ZUNO_MODELS_PATH", models_fixture())
        .env("ZUNO_CONFIG_CONTENT", r#"{"provider":{"anyapi":{}}}"#);
}

#[test]
fn db_query_uses_the_embedded_engine_without_path_binaries() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = zuno();
    command
        .args(["db", "SELECT 1 AS answer", "--format", "json"])
        .env("PATH", "")
        .current_dir(root.path());
    isolated(&mut command, root.path());
    let output = command.output().expect("run db with stripped PATH");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[\n  {\n    \"answer\": 1\n  }\n]\n"
    );
}

#[test]
fn models_verbose_uses_the_public_field_names_and_order() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = zuno();
    command
        .args(["models", "anyapi", "--verbose"])
        .current_dir(root.path());
    isolated(&mut command, root.path());
    configure_models(&mut command);
    let output = command.output().expect("run verbose models");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
    assert_eq!(
        stdout.lines().next(),
        Some("anyapi/anthropic/claude-opus-4-6")
    );
    let keys = [
        "\"id\"",
        "\"providerID\"",
        "\"name\"",
        "\"family\"",
        "\"api\"",
        "\"status\"",
        "\"headers\"",
        "\"options\"",
        "\"cost\"",
        "\"limit\"",
        "\"capabilities\"",
        "\"release_date\"",
        "\"variants\"",
    ];
    let mut previous = 0;
    for key in keys {
        let position = stdout[previous..]
            .find(key)
            .map(|offset| previous + offset)
            .unwrap_or_else(|| panic!("missing verbose key {key}"));
        assert!(position >= previous, "verbose key order changed at {key}");
        previous = position + key.len();
    }
    assert!(!stdout.contains("\"provider_id\""));
}

#[test]
fn models_reports_an_unknown_provider() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = zuno();
    command.args(["models", "missing"]).current_dir(root.path());
    isolated(&mut command, root.path());
    configure_models(&mut command);
    let output = command.output().expect("run missing provider");
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Provider not found: missing\n"
    );
}

#[test]
fn providers_list_and_logout_share_the_auth_store_without_leaking_secrets() {
    let root = tempfile::tempdir().expect("tempdir");
    let auth_path = root.path().join("data/zuno/auth.json");
    std::fs::create_dir_all(auth_path.parent().expect("auth parent")).expect("create auth parent");
    std::fs::write(
        &auth_path,
        r#"{"anyapi":{"type":"api","key":"secret"},"custom":{"type":"oauth","refresh":"r","access":"a","expires":1}}"#,
    )
    .expect("seed credentials");

    let mut list = zuno();
    list.args(["providers", "list"]).current_dir(root.path());
    isolated(&mut list, root.path());
    configure_models(&mut list);
    let listed = list.output().expect("list providers");
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("AnyAPI api"), "{stdout}");
    assert!(stdout.contains("custom oauth"), "{stdout}");
    assert!(stdout.contains("2 credentials"), "{stdout}");
    assert!(
        !stdout.contains("secret"),
        "credentials must not be printed"
    );

    let mut logout = zuno();
    logout
        .args(["providers", "logout", "AnyAPI"])
        .current_dir(root.path());
    isolated(&mut logout, root.path());
    configure_models(&mut logout);
    let removed = logout.output().expect("logout provider");
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let remaining: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&auth_path).expect("read auth")).expect("auth JSON");
    assert!(remaining.get("anyapi").is_none());
    assert!(remaining.get("custom").is_some());
}

#[test]
fn providers_headless_login_reads_the_key_from_stdin() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = zuno();
    command
        .args(["providers", "login", "--provider", "AnyAPI"])
        .current_dir(root.path())
        .stdin(Stdio::piped());
    isolated(&mut command, root.path());
    configure_models(&mut command);
    let mut child = command.spawn().expect("spawn provider login");
    child
        .stdin
        .take()
        .expect("login stdin")
        .write_all(b"headless-secret\n")
        .expect("write API key");
    let output = child.wait_with_output().expect("wait for login");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let auth: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("data/zuno/auth.json")).expect("read auth"),
    )
    .expect("auth JSON");
    assert_eq!(auth["anyapi"]["type"], "api");
    assert_eq!(auth["anyapi"]["key"], "headless-secret");
}

#[test]
fn auth_methods_expose_openai_oauth_without_leaking_it_to_custom_providers() {
    let root = tempfile::tempdir().expect("tempdir");

    let mut openai = zuno();
    openai
        .args(["auth", "methods", "openai"])
        .current_dir(root.path());
    isolated(&mut openai, root.path());
    configure_models(&mut openai);
    let output = openai.output().expect("list OpenAI methods");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for method in ["chatgpt-browser", "chatgpt-device", "api-key"] {
        assert!(stdout.contains(method), "{stdout}");
    }

    let mut custom = zuno();
    custom
        .args(["auth", "methods", "myopenai"])
        .current_dir(root.path());
    isolated(&mut custom, root.path());
    configure_models(&mut custom);
    let output = custom.output().expect("list custom methods");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("api-key"), "{stdout}");
    assert!(!stdout.contains("chatgpt-"), "{stdout}");
}

#[test]
fn auth_login_accepts_a_positional_provider_and_explicit_api_key_method() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = zuno();
    command
        .args(["auth", "login", "AnyAPI", "--method", "api-key"])
        .current_dir(root.path())
        .stdin(Stdio::piped());
    isolated(&mut command, root.path());
    configure_models(&mut command);
    let mut child = command.spawn().expect("spawn positional provider login");
    child
        .stdin
        .take()
        .expect("login stdin")
        .write_all(b"positional-secret\n")
        .expect("write API key");
    let output = child.wait_with_output().expect("wait for login");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let auth: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("data/zuno/auth.json")).expect("read auth"),
    )
    .expect("auth JSON");
    assert_eq!(auth["anyapi"]["key"], "positional-secret");
}

#[test]
fn mcp_add_list_auth_and_logout_persist_headless_state() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut add = zuno();
    add.args([
        "mcp",
        "add",
        "docs",
        "--url",
        "https://example.com/mcp",
        "--header",
        "Authorization=Bearer test",
    ])
    .current_dir(root.path());
    isolated(&mut add, root.path());
    let added = add.output().expect("add MCP server");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let config_path = root.path().join("config/zuno/zuno.json");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).expect("read generated config"))
            .expect("config JSON");
    assert_eq!(config["mcp"]["docs"]["type"], "remote");
    assert_eq!(config["mcp"]["docs"]["url"], "https://example.com/mcp");
    assert_eq!(
        config["mcp"]["docs"]["headers"]["Authorization"],
        "Bearer test"
    );

    let listed = run(root.path(), &["mcp", "list"]);
    assert!(listed.status.success());
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("docs not initialized"), "{stdout}");
    assert!(stdout.contains("https://example.com/mcp"), "{stdout}");

    let auth_path = root.path().join("data/zuno/mcp-auth.json");
    std::fs::create_dir_all(auth_path.parent().expect("MCP auth parent"))
        .expect("create MCP auth parent");
    std::fs::write(
        &auth_path,
        r#"{"docs":{"tokens":{"accessToken":"secret","refreshToken":"refresh","expiresAt":4102444800000},"serverUrl":"https://example.com/mcp"}}"#,
    )
    .expect("seed MCP auth");

    let auth_listed = run(root.path(), &["mcp", "auth", "list"]);
    assert!(auth_listed.status.success());
    let stdout = String::from_utf8_lossy(&auth_listed.stdout);
    assert!(stdout.contains("docs authenticated"), "{stdout}");
    assert!(!stdout.contains("secret"));
    assert!(!stdout.contains("refresh"));

    let logged_out = run(root.path(), &["mcp", "logout", "docs"]);
    assert!(logged_out.status.success());
    let auth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(auth_path).expect("read MCP auth after logout"))
            .expect("MCP auth JSON");
    assert!(auth.as_object().is_some_and(serde_json::Map::is_empty));
}

#[test]
fn debug_config_emits_only_resolved_json() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = zuno();
    command
        .args(["debug", "config"])
        .current_dir(root.path())
        .env(
            "ZUNO_CONFIG_CONTENT",
            r#"{"username":"debug-user","share":"disabled","web_search":{"provider":"exa","max_queries":3,"max_results":7,"timeout_ms":12000}}"#,
        );
    isolated(&mut command, root.path());
    let output = command.output().expect("debug config");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout contains only JSON");
    assert_eq!(config["username"], "debug-user");
    assert_eq!(config["share"], "disabled");
    assert_eq!(
        config["web_search"],
        serde_json::json!({
            "provider": "exa",
            "max_queries": 3,
            "max_results": 7,
            "timeout_ms": 12000
        })
    );
}

#[test]
fn debug_config_includes_runtime_markdown_agents_and_commands() {
    let root = tempfile::tempdir().expect("tempdir");
    let config_dir = root.path().join("config/zuno");
    let agent_file = config_dir.join("agent/powerapps/runtime-agent.md");
    let command_file = config_dir.join("command/runtime-command.md");
    std::fs::create_dir_all(agent_file.parent().expect("agent parent")).expect("agent directory");
    std::fs::create_dir_all(command_file.parent().expect("command parent"))
        .expect("command directory");
    std::fs::write(
        agent_file,
        "---\ndescription: runtime agent\nmode: subagent\n---\nRuntime agent prompt.\n",
    )
    .expect("agent markdown");
    std::fs::write(
        command_file,
        "---\ndescription: runtime command\nagent: build\n---\nRun $ARGUMENTS.\n",
    )
    .expect("command markdown");

    let mut command = zuno();
    command.args(["debug", "config"]).current_dir(root.path());
    isolated(&mut command, root.path());
    let output = command.output().expect("debug config");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout contains only JSON");
    assert_eq!(
        config["agent"]["powerapps/runtime-agent"]["prompt"],
        "Runtime agent prompt."
    );
    assert_eq!(
        config["command"]["runtime-command"]["template"],
        "Run $ARGUMENTS."
    );
}

const EXPORT_SESSION: &str = "ses_behaviorexport000000000000ab";
const EXPORT_MESSAGE: &str = "msg_behaviorexport000000000000us";
const EXPORT_PART: &str = "prt_behaviorexport00000000000001";

fn seed_export_database(path: &Path) {
    use zuno_db::message::{MessageRecord, MessageStore, PartRecord};

    std::fs::create_dir_all(path.parent().expect("database parent")).expect("database directory");
    let mut connection = zuno_db::open::open_at(path).expect("create export database");
    zuno_db::migration::apply(&mut connection).expect("apply schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('prj_behavior_export', '/srv/export', 1, 1, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, path, title, version, cost, tokens_input, \
                tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, agent, \
                time_created, time_updated) \
             VALUES ('{EXPORT_SESSION}', 'prj_behavior_export', 'export', '/srv/export', '', \
                     'CLI export round trip', '1.18.13', 0, 0, 0, 0, 0, 0, 'build', 2, 3);"
        ))
        .expect("seed project and session");
    let store = MessageStore::new(&connection);
    let message = MessageRecord::from_json(serde_json::json!({
        "id": EXPORT_MESSAGE,
        "sessionID": EXPORT_SESSION,
        "role": "user",
        "time": { "created": 4_i64 },
        "agent": "build",
        "model": { "providerID": "test", "modelID": "test-model" }
    }))
    .expect("split message");
    store.put_message_at(&message, 4).expect("write message");
    let part = PartRecord::from_json(
        serde_json::json!({
            "id": EXPORT_PART,
            "sessionID": EXPORT_SESSION,
            "messageID": EXPORT_MESSAGE,
            "type": "text",
            "text": "round-trip me"
        }),
        5,
    )
    .expect("split part");
    store.put_part_at(&part, 5).expect("write part");
}

fn export_document(root: &Path, database: &Path) -> serde_json::Value {
    let mut command = zuno();
    command
        .args(["export", EXPORT_SESSION])
        .current_dir(root)
        .env("ZUNO_DB", database);
    isolated(&mut command, root);
    let output = command.output().expect("run export");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("export JSON")
}

#[test]
fn export_then_import_restores_the_transcript_in_another_database() {
    let root = tempfile::tempdir().expect("tempdir");
    let source = root.path().join("source.db");
    seed_export_database(&source);
    let exported = export_document(root.path(), &source);
    let file = root.path().join("exported.json");
    std::fs::write(
        &file,
        serde_json::to_vec_pretty(&exported).expect("render export"),
    )
    .expect("write export");

    let target = root.path().join("target.db");
    let mut command = zuno();
    command
        .args(["import", &file.to_string_lossy()])
        .current_dir(root.path())
        .env("ZUNO_DB", &target);
    isolated(&mut command, root.path());
    let output = command.output().expect("run import");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(EXPORT_SESSION));

    let restored = export_document(root.path(), &target);
    assert_eq!(restored["info"]["id"], EXPORT_SESSION);
    assert_eq!(restored["info"]["title"], "CLI export round trip");
    assert_eq!(restored["messages"][0]["info"]["id"], EXPORT_MESSAGE);
    assert_eq!(restored["messages"][0]["parts"][0]["id"], EXPORT_PART);
    assert_eq!(restored["messages"][0]["parts"][0]["text"], "round-trip me");
}

#[test]
fn export_without_a_session_id_explains_the_required_selection() {
    let root = tempfile::tempdir().expect("tempdir");
    let output = run(root.path(), &["export"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session id"), "{stderr}");
    assert!(stderr.contains("session list"), "{stderr}");
    assert!(!stderr.contains("pending"), "{stderr}");
}

#[test]
fn export_reports_an_unknown_session() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("export.db");
    seed_export_database(&database);
    let missing = "ses_behaviorexport000000000000zz";
    let mut command = zuno();
    command
        .args(["export", missing])
        .current_dir(root.path())
        .env("ZUNO_DB", database);
    isolated(&mut command, root.path());
    let output = command.output().expect("run export");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("Session not found: {missing}")),
        "{stderr}"
    );
}

#[test]
fn import_reports_a_missing_file() {
    let root = tempfile::tempdir().expect("tempdir");
    let output = run(root.path(), &["import", "/definitely/absent/export.json"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("File not found"), "{stderr}");
}

#[test]
fn an_opencode_named_file_is_not_part_of_the_zuno_config_graph() {
    let root = tempfile::tempdir().expect("tempdir");
    let unrelated = root.path().join("config/zuno/opencode.json");
    std::fs::create_dir_all(unrelated.parent().expect("parent")).expect("config dir");
    std::fs::write(&unrelated, r#"{"username":"not-zuno-input"}"#).expect("seed file");

    let output = run(root.path(), &["debug", "config"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("not-zuno-input"),
        "an unrelated product filename entered Zuno config: {stdout}"
    );
}

/// The same config under the canonical filename is read, and its setting takes
/// effect — the other half of the pair, so a rename that broke reading as well as
/// rejecting could not pass.
#[test]
fn a_canonically_named_config_takes_effect() {
    let root = tempfile::tempdir().expect("tempdir");
    let config = root.path().join("config/zuno/zuno.json");
    std::fs::create_dir_all(config.parent().expect("parent")).expect("config dir");
    std::fs::write(&config, r#"{"username":"canonical-name-reader"}"#).expect("seed config");

    let output = run(root.path(), &["debug", "config"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("canonical-name-reader"),
        "the canonically named config was accepted but its setting did not appear: {stdout}"
    );
}
