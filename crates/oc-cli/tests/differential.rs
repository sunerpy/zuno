use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

/// Long options this port adds beyond the oracle's, per command.
///
/// The surface check below is an **equality**, so an addition has to be declared
/// here or the test fails. That is the point: equality catches a dropped upstream
/// flag *and* a flag that appeared without anyone deciding to add it, which a
/// one-directional superset check would wave through.
///
/// `session list` is the one entry. Upstream's listing cannot leave the current
/// project (`session/session.ts:548-555` injects the id unconditionally), so
/// every flag that makes a cross-project listing expressible is necessarily
/// absent from its help. `--limit` carries upstream's `--max-count` as a visible
/// alias, so both names appear and the upstream spelling still works.
const ADDED_LONG_FLAGS: &[(&[&str], &[&str])] = &[(
    &["session", "list"],
    &[
        "all-projects",
        "project",
        "archived",
        "roots",
        "no-roots",
        "sort",
        "limit",
    ],
)];

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

    let mut expected = long_flags(&oracle_help);
    let added: BTreeSet<String> = ADDED_LONG_FLAGS
        .iter()
        .find(|(command, _)| *command == args)
        .map(|(_, flags)| flags.iter().map(|flag| (*flag).to_owned()).collect())
        .unwrap_or_default();
    expected.extend(added.iter().cloned());

    assert_eq!(
        long_flags(&actual_help),
        expected,
        "long-option mismatch for {}\ndeclared additions: {added:?}\nRust:\n{}\nOracle:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&actual_help),
        String::from_utf8_lossy(&oracle_help),
    );
}

#[test]
fn every_declared_flag_addition_is_actually_present_and_upstream_keeps_its_own() {
    let root = tempfile::tempdir().expect("tempdir");
    for (args, added) in ADDED_LONG_FLAGS {
        let mut with_help = args.to_vec();
        with_help.push("--help");
        let actual = run(&rust_path(), &with_help, root.path());
        let flags = long_flags(&[actual.stdout.as_slice(), actual.stderr.as_slice()].concat());
        for flag in *added {
            assert!(
                flags.contains(*flag),
                "{} declares --{flag} as an addition but does not offer it",
                args.join(" ")
            );
        }
        let oracle = run(Path::new(ORACLE), &with_help, root.path());
        let upstream = long_flags(&[oracle.stdout.as_slice(), oracle.stderr.as_slice()].concat());
        for flag in &upstream {
            assert!(
                flags.contains(flag),
                "{} dropped upstream's --{flag}",
                args.join(" ")
            );
        }
        assert!(
            upstream.contains("max-count"),
            "the fixture assumption changed: upstream's session list no longer offers --max-count"
        );
    }
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

// ---------------------------------------------------------------------------
// `session list --all-projects` against the documented endpoint
//
// `/experimental/session` (`groups/experimental.ts:224-233`) is the only place
// upstream publishes a cross-project listing, and it returns exactly the shape
// the CLI now emits. Both sides are pointed at one database file through
// `OPENCODE_DB` — the real binary resolves it in `core/src/database/database.ts:44-47`
// and `oc_paths::db_path` honours the same variable — so the comparison is a set
// equality on the same rows rather than two independently seeded stores.
//
// The two `roots` pairings are compared separately because the defaults differ
// and neither is wrong: the CLI defaults to roots-only, which is what upstream's
// own `session list` hard-codes (`cli/cmd/session.ts:87`), while the endpoint's
// `roots` query parameter is unset by default.
// ---------------------------------------------------------------------------

struct FixtureSession {
    id: &'static str,
    project: &'static str,
    parent: Option<&'static str>,
    created: i64,
    updated: i64,
    archived: Option<i64>,
}

/// Every session the fixture seeds: three projects, two parent/child pairs, and
/// one archived root so `--archived` has something to widen the result with.
const FIXTURE_SESSIONS: &[FixtureSession] = &[
    FixtureSession {
        id: "ses_dx_one_root",
        project: "prj_dx_one",
        parent: None,
        created: 1_000,
        updated: 5_000,
        archived: None,
    },
    FixtureSession {
        id: "ses_dx_one_kid",
        project: "prj_dx_one",
        parent: Some("ses_dx_one_root"),
        created: 1_100,
        updated: 4_500,
        archived: None,
    },
    FixtureSession {
        id: "ses_dx_one_arch",
        project: "prj_dx_one",
        parent: None,
        created: 1_200,
        updated: 4_800,
        archived: Some(4_900),
    },
    FixtureSession {
        id: "ses_dx_two_root",
        project: "prj_dx_two",
        parent: None,
        created: 3_000,
        updated: 4_000,
        archived: None,
    },
    FixtureSession {
        id: "ses_dx_two_kid",
        project: "prj_dx_two",
        parent: Some("ses_dx_two_root"),
        created: 3_100,
        updated: 3_500,
        archived: None,
    },
    FixtureSession {
        id: "ses_dx_three_root",
        project: "prj_dx_three",
        parent: None,
        created: 4_000,
        updated: 3_000,
        archived: None,
    },
];

/// Seed a database the oracle and the Rust CLI will both open.
///
/// Written with `rusqlite`, which `oc-cli` already depends on, so the fixture
/// does not depend on either binary being able to write it.
fn seed_shared_database(path: &Path) {
    let mut connection = rusqlite::Connection::open(path).expect("create the shared database");
    oc_db::migration::apply(&mut connection).expect("apply the schema");
    for (id, worktree, name) in [
        ("prj_dx_one", "/srv/dx-one", Some("DX One")),
        ("prj_dx_two", "/srv/dx-two", None),
        ("prj_dx_three", "/srv/dx-three", Some("DX Three")),
    ] {
        connection
            .execute(
                "INSERT INTO project (id, worktree, name, time_created, time_updated, sandboxes) \
                 VALUES (?1, ?2, ?3, 1, 1, '[]')",
                rusqlite::params![id, worktree, name],
            )
            .expect("insert project");
    }
    for seed in FIXTURE_SESSIONS {
        connection
            .execute(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, path, title, \
                 version, cost, tokens_input, tokens_output, tokens_reasoning, \
                 tokens_cache_read, tokens_cache_write, agent, time_created, time_updated, \
                 time_archived) \
                 VALUES (?1, ?2, ?3, ?4, ?5, '', ?6, '1.18.13', 2.0, 10, 20, 0, 0, 0, 'build', \
                 ?7, ?8, ?9)",
                rusqlite::params![
                    seed.id,
                    seed.project,
                    seed.parent,
                    seed.id.trim_start_matches("ses_"),
                    format!("/srv/{}", seed.project),
                    format!("Title for {}", seed.id),
                    seed.created,
                    seed.updated,
                    seed.archived,
                ],
            )
            .expect("insert session");
    }
}

/// Start the oracle's HTTP server on `port`, waiting for it to say so.
fn spawn_oracle_server(root: &Path, database: &Path, port: u16) -> Option<Child> {
    let mut command = Command::new(ORACLE);
    command
        .args([
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolated(&mut command, root);
    command.env("OPENCODE_DB", database);
    let mut child = command.spawn().expect("spawn the oracle server");

    let stdout = child.stdout.take().expect("oracle stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.contains("listening on") => return Some(child),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// `GET` one path off the oracle server and parse it as JSON.
///
/// Written on a raw socket rather than through `reqwest::blocking`, which is not
/// in this workspace's feature set — and enabling it would edit a manifest five
/// other crates share. A loopback HTTP/1.1 request with `Connection: close` is
/// small enough to be obvious, and it also sidesteps the ambient `http_proxy`
/// this machine exports, which silently swallows loopback requests.
fn oracle_json(port: u16, query: &str) -> serde_json::Value {
    let target = format!("/experimental/session{query}");
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\n\
         Connection: close\r\n\r\n"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                use std::io::Write as _;
                stream
                    .set_read_timeout(Some(Duration::from_secs(20)))
                    .expect("set a read timeout");
                stream
                    .write_all(request.as_bytes())
                    .expect("send the request");
                return read_http_json(&mut stream, &target);
            }
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "{target}: could not connect: {error}"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Read one HTTP/1.1 response off `stream` and parse its body as JSON.
///
/// The framing is read rather than assumed: the oracle's server does not close
/// the socket on `Connection: close`, so reading to EOF blocks until the read
/// timeout expires. `Content-Length` bounds the body, and the chunked branch is
/// there because the same server answers some routes with SSE-style framing.
fn read_http_json(stream: &mut std::net::TcpStream, target: &str) -> serde_json::Value {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read a header line");
        assert!(read > 0, "{target}: the connection closed mid-header");
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push_str(&line);
    }

    let status = head.lines().next().unwrap_or_default();
    assert!(status.contains(" 200 "), "{target} -> {status} ({head})");

    let header = |name: &str| {
        head.lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with(&format!("{}:", name.to_ascii_lowercase()))
            })
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_owned())
    };

    let payload = if header("transfer-encoding").is_some_and(|value| value.contains("chunked")) {
        dechunk(&mut reader, target)
    } else {
        let length: usize = header("content-length")
            .unwrap_or_else(|| panic!("{target}: neither Content-Length nor chunked ({head})"))
            .parse()
            .expect("a numeric Content-Length");
        let mut body = vec![0_u8; length];
        std::io::Read::read_exact(&mut reader, &mut body).expect("read the body");
        String::from_utf8(body).expect("a UTF-8 body")
    };

    serde_json::from_str(&payload)
        .unwrap_or_else(|error| panic!("{target}: {error} in body {payload}"))
}

/// Reassemble an HTTP/1.1 chunked body.
fn dechunk(reader: &mut BufReader<&mut std::net::TcpStream>, target: &str) -> String {
    let mut out = String::new();
    loop {
        let mut size_line = String::new();
        let read = reader.read_line(&mut size_line).expect("read a chunk size");
        assert!(read > 0, "{target}: the connection closed mid-chunk");
        let size =
            usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0").trim(), 16)
                .unwrap_or(0);
        if size == 0 {
            return out;
        }
        let mut chunk = vec![0_u8; size + 2];
        std::io::Read::read_exact(reader, &mut chunk).expect("read a chunk");
        chunk.truncate(size);
        out.push_str(&String::from_utf8(chunk).expect("a UTF-8 chunk"));
    }
}

/// Run the Rust CLI's JSON listing against the shared database.
fn rust_listing(root: &Path, database: &Path, args: &[&str]) -> serde_json::Value {
    let mut command = rust_binary();
    command.args(args);
    isolated(&mut command, root);
    command.env("OPENCODE_DB", database);
    let output = command.output().expect("run the Rust session listing");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout contains only JSON")
}

#[test]
fn session_list_all_projects_matches_the_experimental_endpoint_on_one_database() {
    if !Path::new(ORACLE).is_file() {
        eprintln!(
            "SKIPPED session_list_all_projects_matches_the_experimental_endpoint_on_one_database: \
             the real opencode binary is absent at {ORACLE}; the cross-project listing was NOT \
             compared against /experimental/session"
        );
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("shared.db");
    seed_shared_database(&database);

    // A port the OS hands out, released before the oracle claims it. A fixed
    // port would collide with a sibling test run; binding to 0 and letting the
    // oracle inherit the socket is not something its CLI supports.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
        listener
            .local_addr()
            .expect("read the reserved port")
            .port()
    };

    let Some(mut server) = spawn_oracle_server(root.path(), &database, port) else {
        eprintln!(
            "SKIPPED session_list_all_projects_matches_the_experimental_endpoint_on_one_database: \
             the real opencode binary never reported `listening on`; the cross-project listing was \
             NOT compared against /experimental/session"
        );
        return;
    };

    let comparisons = [
        (
            "roots-only",
            vec!["session", "list", "--all-projects", "--format", "json"],
            "?roots=true",
            3,
        ),
        (
            "including children",
            vec![
                "session",
                "list",
                "--all-projects",
                "--no-roots",
                "--format",
                "json",
            ],
            "",
            5,
        ),
        (
            "including archived",
            vec![
                "session",
                "list",
                "--all-projects",
                "--no-roots",
                "--archived",
                "--format",
                "json",
            ],
            "?archived=true",
            6,
        ),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (label, args, query, expected) in comparisons {
        let oracle = oracle_json(port, query);
        let actual = rust_listing(root.path(), &database, &args);

        let oracle_rows = oracle.as_array().expect("the endpoint returns an array");
        let actual_rows = actual.as_array().expect("the CLI returns an array");
        if oracle_rows.len() != expected || actual_rows.len() != expected {
            failures.push(format!(
                "{label}: expected {expected} rows, oracle gave {}, Rust gave {}",
                oracle_rows.len(),
                actual_rows.len()
            ));
        }
        if actual != oracle {
            failures.push(format!(
                "{label}: sets differ\nRust:\n{}\nOracle:\n{}",
                serde_json::to_string_pretty(&actual).expect("render"),
                serde_json::to_string_pretty(&oracle).expect("render"),
            ));
        }
        // Byte parity too, not only semantic equality: `serde_json` renders an
        // integral f64 as `2.0` where `JSON.stringify` writes `2`, and that was
        // the one textual difference this comparison found. Dropping the check
        // would let it come back invisibly.
        let actual_text = serde_json::to_string(&actual).expect("render");
        let oracle_text = serde_json::to_string(&oracle).expect("render");
        if actual_text != oracle_text {
            failures.push(format!(
                "{label}: JSON text differs\nRust:  {actual_text}\nOracle:{oracle_text}"
            ));
        }
    }

    let _ = server.kill();
    let _ = server.wait();
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
