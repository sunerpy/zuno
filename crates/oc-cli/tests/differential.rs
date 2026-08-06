use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ORACLE: &str = "/config/.local/share/mise/installs/opencode/1.18.12/opencode";

fn rust_binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_opencode-rust"))
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
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "true");
}

fn run(binary: &Path, args: &[&str], root: &Path) -> Output {
    let mut command = Command::new(binary);
    command.args(args);
    isolated(&mut command, root);
    command.output().expect("run CLI")
}

fn rust_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_opencode-rust"))
}

fn models_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../oc-llm/tests/fixtures/models-dev-pinned.json")
}

fn configure_models(command: &mut Command) {
    command
        .env("OPENCODE_MODELS_PATH", models_fixture())
        .env("OPENCODE_CONFIG_CONTENT", r#"{"provider":{"anyapi":{}}}"#);
}

fn long_flags(output: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(output)
        .split_ascii_whitespace()
        .filter_map(|word| {
            let start = word.find("--")? + 2;
            let flag: String = word[start..]
                .chars()
                .take_while(|character| character.is_ascii_alphanumeric() || *character == '-')
                .collect();
            (!flag.is_empty()).then_some(flag)
        })
        .collect()
}

fn assert_help_flags(args: &[&str]) {
    let root = tempfile::tempdir().expect("tempdir");
    let mut oracle_args = args.to_vec();
    oracle_args.push("--help");
    let oracle = run(Path::new(ORACLE), &oracle_args, root.path());
    let actual = run(&rust_path(), &oracle_args, root.path());
    let oracle_help = [oracle.stdout.as_slice(), oracle.stderr.as_slice()].concat();
    let actual_help = [actual.stdout.as_slice(), actual.stderr.as_slice()].concat();
    assert!(
        oracle.status.success(),
        "oracle help failed: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    assert!(
        actual.status.success(),
        "Rust help failed: {}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        long_flags(&actual_help),
        long_flags(&oracle_help),
        "long-option mismatch for {}\nRust:\n{}\nOracle:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&actual_help),
        String::from_utf8_lossy(&oracle_help),
    );
}

#[test]
fn every_headless_command_keeps_the_oracle_long_option_surface() {
    for args in [
        &["run"][..],
        &["serve"][..],
        &["session"][..],
        &["session", "list"][..],
        &["session", "delete"][..],
        &["agent"][..],
        &["agent", "list"][..],
        &["agent", "create"][..],
        &["models"][..],
        &["providers"][..],
        &["providers", "list"][..],
        &["providers", "login"][..],
        &["providers", "logout"][..],
        &["auth"][..],
        &["mcp"][..],
        &["mcp", "list"][..],
        &["mcp", "add"][..],
        &["mcp", "auth"][..],
        &["mcp", "logout"][..],
        &["mcp", "debug"][..],
        &["db"][..],
        &["debug"][..],
        &["debug", "paths"][..],
        &["debug", "config"][..],
        &["debug", "agent"][..],
        &["debug", "skill"][..],
        &["debug", "rg"][..],
        &["debug", "lsp"][..],
        &["debug", "snapshot"][..],
    ] {
        assert_help_flags(args);
    }
}

#[test]
fn db_query_matches_oracle_in_json_and_tsv() {
    let root = tempfile::tempdir().expect("tempdir");
    let query = "SELECT 1 AS answer, 'hello' AS greeting";
    for format in ["json", "tsv"] {
        let args = ["db", query, "--format", format];
        let oracle = run(Path::new(ORACLE), &args, root.path());
        let actual = run(&rust_path(), &args, root.path());
        assert_eq!(
            actual.status.success(),
            oracle.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, oracle.stdout, "{format} stdout");
        if format == "json" {
            serde_json::from_slice::<serde_json::Value>(&actual.stdout)
                .expect("JSON mode must emit only JSON to stdout");
        }
    }
}

#[test]
fn db_query_does_not_need_sqlite3_or_any_other_path_binary() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = rust_binary();
    command
        .args(["db", "SELECT 1 AS answer", "--format", "json"])
        .env("PATH", "");
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
fn models_listing_and_provider_filter_match_oracle() {
    for args in [&["models"][..], &["models", "anyapi"][..]] {
        let root = tempfile::tempdir().expect("tempdir");
        let mut oracle = Command::new(ORACLE);
        oracle.args(args);
        isolated(&mut oracle, root.path());
        configure_models(&mut oracle);

        let mut actual = rust_binary();
        actual.args(args);
        isolated(&mut actual, root.path());
        configure_models(&mut actual);

        let oracle = oracle.output().expect("run oracle models");
        let actual = actual.output().expect("run Rust models");
        assert!(
            oracle.status.success(),
            "{}",
            String::from_utf8_lossy(&oracle.stderr)
        );
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, oracle.stdout, "{}", args.join(" "));
    }
}

#[test]
fn models_verbose_uses_the_upstream_field_names_and_order() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = rust_binary();
    command.args(["models", "anyapi", "--verbose"]);
    isolated(&mut command, root.path());
    configure_models(&mut command);
    let output = command.output().expect("run verbose models");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
    let first_line = stdout.lines().next().expect("qualified model id");
    assert_eq!(first_line, "anyapi/anthropic/claude-opus-4-6");
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
fn models_reports_an_unknown_provider_like_the_oracle() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = rust_binary();
    command.args(["models", "missing"]);
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
fn providers_list_and_logout_use_the_shared_auth_store() {
    let root = tempfile::tempdir().expect("tempdir");
    let auth_path = root.path().join("data/opencode/auth.json");
    std::fs::create_dir_all(auth_path.parent().expect("auth parent")).expect("create auth parent");
    std::fs::write(
        &auth_path,
        r#"{"anyapi":{"type":"api","key":"secret"},"custom":{"type":"oauth","refresh":"r","access":"a","expires":1}}"#,
    )
    .expect("seed credentials");

    let mut list = rust_binary();
    list.args(["providers", "list"]);
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
        "credentials must never be printed"
    );

    let mut logout = rust_binary();
    logout.args(["providers", "logout", "AnyAPI"]);
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
    let mut command = rust_binary();
    command.args(["providers", "login", "--provider", "AnyAPI"]);
    isolated(&mut command, root.path());
    configure_models(&mut command);
    command.stdin(Stdio::piped());
    let mut child = command.spawn().expect("spawn provider login");
    use std::io::Write as _;
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

    let auth_path = root.path().join("data/opencode/auth.json");
    let auth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(auth_path).expect("read auth")).expect("auth JSON");
    assert_eq!(auth["anyapi"]["type"], "api");
    assert_eq!(auth["anyapi"]["key"], "headless-secret");
}

#[test]
fn mcp_add_list_and_logout_persist_headless_state() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut add = rust_binary();
    add.args([
        "mcp",
        "add",
        "docs",
        "--url",
        "https://example.com/mcp",
        "--header",
        "Authorization=Bearer test",
    ]);
    isolated(&mut add, root.path());
    let added = add.output().expect("add MCP server");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let config_path = root.path().join("config/opencode/opencode.json");
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).expect("read generated config"))
            .expect("config JSON");
    assert_eq!(config["mcp"]["docs"]["type"], "remote");
    assert_eq!(config["mcp"]["docs"]["url"], "https://example.com/mcp");
    assert_eq!(
        config["mcp"]["docs"]["headers"]["Authorization"],
        "Bearer test"
    );

    let mut list = rust_binary();
    list.args(["mcp", "list"]);
    isolated(&mut list, root.path());
    let listed = list.output().expect("list MCP servers");
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("docs not initialized"), "{stdout}");
    assert!(stdout.contains("https://example.com/mcp"), "{stdout}");

    let auth_path = root.path().join("data/opencode/mcp-auth.json");
    std::fs::create_dir_all(auth_path.parent().expect("MCP auth parent"))
        .expect("create MCP auth parent");
    std::fs::write(
        &auth_path,
        r#"{"docs":{"tokens":{"accessToken":"secret","refreshToken":"refresh","expiresAt":4102444800000},"serverUrl":"https://example.com/mcp"}}"#,
    )
    .expect("seed MCP auth");

    let mut auth_list = rust_binary();
    auth_list.args(["mcp", "auth", "list"]);
    isolated(&mut auth_list, root.path());
    let auth_listed = auth_list.output().expect("list MCP auth");
    assert!(
        auth_listed.status.success(),
        "{}",
        String::from_utf8_lossy(&auth_listed.stderr)
    );
    let stdout = String::from_utf8_lossy(&auth_listed.stdout);
    assert!(stdout.contains("docs authenticated"), "{stdout}");
    assert!(!stdout.contains("secret"));
    assert!(!stdout.contains("refresh"));

    let mut logout = rust_binary();
    logout.args(["mcp", "logout", "docs"]);
    isolated(&mut logout, root.path());
    let logged_out = logout.output().expect("logout MCP");
    assert!(
        logged_out.status.success(),
        "{}",
        String::from_utf8_lossy(&logged_out.stderr)
    );
    let auth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(auth_path).expect("read MCP auth after logout"))
            .expect("MCP auth JSON");
    assert!(auth.as_object().is_some_and(serde_json::Map::is_empty));
}

#[test]
fn debug_paths_matches_oracle_exactly() {
    let root = tempfile::tempdir().expect("tempdir");
    let oracle = run(Path::new(ORACLE), &["debug", "paths"], root.path());
    let actual = run(&rust_path(), &["debug", "paths"], root.path());
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, oracle.stdout);
}

#[test]
fn debug_config_emits_only_resolved_json() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = rust_binary();
    command.args(["debug", "config"]);
    isolated(&mut command, root.path());
    command.env(
        "OPENCODE_CONFIG_CONTENT",
        r#"{"username":"debug-user","share":"disabled"}"#,
    );
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
}
