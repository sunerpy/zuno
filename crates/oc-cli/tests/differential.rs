use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use oc_testkit::{PinnedOracle, pinned_oracle, pinned_oracle_or_skip};

/// The installed release every comparison in this file runs against.
///
/// Resolved and screened centrally rather than written down: a path naming one
/// release can select a build other than the pinned one, and it exists on exactly
/// one machine. Absence panics here because these comparisons have no meaning
/// without the oracle and previously died in `Command::output` anyway; the tests
/// that must survive a machine without it gate on [`pinned_oracle_or_skip`] instead.
fn oracle() -> &'static Path {
    match pinned_oracle() {
        PinnedOracle::Found(program) => program.as_path(),
        PinnedOracle::Absent(reason) | PinnedOracle::Disagrees(reason) => panic!("{reason}"),
    }
}

fn rust_binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zuno"))
}

fn isolated_base(command: &mut Command, root: &Path) {
    command
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("HOME", root.join("home"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"));
}

fn isolated_oracle(command: &mut Command, root: &Path) {
    isolated_base(command, root);
    command
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "true");
}

fn isolated_subject(command: &mut Command, root: &Path) {
    isolated_base(command, root);
    command
        .env("ZUNO_DISABLE_AUTOUPDATE", "true")
        .env("ZUNO_DISABLE_MODELS_FETCH", "true")
        .env("ZUNO_DISABLE_DEFAULT_PLUGINS", "true")
        .env("ZUNO_DISABLE_LSP_DOWNLOAD", "true");
}

fn isolated_for_binary(command: &mut Command, binary: &Path, root: &Path) {
    if binary == rust_path() {
        isolated_subject(command, root);
    } else {
        isolated_oracle(command, root);
    }
}

fn run(binary: &Path, args: &[&str], root: &Path) -> Output {
    let mut command = Command::new(binary);
    command.args(args);
    isolated_for_binary(&mut command, binary, root);
    command.output().expect("run CLI")
}

fn rust_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno"))
}

fn models_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../oc-llm/tests/fixtures/models-dev-pinned.json")
}

fn configure_oracle_models(command: &mut Command) {
    command
        .env("OPENCODE_MODELS_PATH", models_fixture())
        .env("OPENCODE_CONFIG_CONTENT", r#"{"provider":{"anyapi":{}}}"#);
}

fn configure_subject_models(command: &mut Command) {
    command
        .env("ZUNO_MODELS_PATH", models_fixture())
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
    let oracle = run(oracle(), &oracle_args, root.path());
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
        let oracle = run(oracle(), &with_help, root.path());
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
        &["export"][..],
        &["import"][..],
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
        let oracle = run(oracle(), &args, root.path());
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
    isolated_subject(&mut command, root.path());
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
        let mut oracle = Command::new(oracle());
        oracle.args(args);
        isolated_oracle(&mut oracle, root.path());
        configure_oracle_models(&mut oracle);

        let mut actual = rust_binary();
        actual.args(args);
        isolated_subject(&mut actual, root.path());
        configure_subject_models(&mut actual);

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
    isolated_subject(&mut command, root.path());
    configure_subject_models(&mut command);
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
    isolated_subject(&mut command, root.path());
    configure_subject_models(&mut command);
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
    let auth_path = root.path().join("data/zuno/auth.json");
    std::fs::create_dir_all(auth_path.parent().expect("auth parent")).expect("create auth parent");
    std::fs::write(
        &auth_path,
        r#"{"anyapi":{"type":"api","key":"secret"},"custom":{"type":"oauth","refresh":"r","access":"a","expires":1}}"#,
    )
    .expect("seed credentials");

    let mut list = rust_binary();
    list.args(["providers", "list"]);
    isolated_subject(&mut list, root.path());
    configure_subject_models(&mut list);
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
    isolated_subject(&mut logout, root.path());
    configure_subject_models(&mut logout);
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
    isolated_subject(&mut command, root.path());
    configure_subject_models(&mut command);
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

    let auth_path = root.path().join("data/zuno/auth.json");
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
    isolated_subject(&mut add, root.path());
    let added = add.output().expect("add MCP server");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let config_path = root.path().join("config/zuno/opencode.json");
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
    isolated_subject(&mut list, root.path());
    let listed = list.output().expect("list MCP servers");
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
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

    let mut auth_list = rust_binary();
    auth_list.args(["mcp", "auth", "list"]);
    isolated_subject(&mut auth_list, root.path());
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
    isolated_subject(&mut logout, root.path());
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
    let oracle = run(oracle(), &["debug", "paths"], root.path());
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
    let expected = String::from_utf8(oracle.stdout)
        .expect("oracle paths are UTF-8")
        .replace("/opencode", "/zuno")
        .into_bytes();
    assert_eq!(actual.stdout, expected);
}

#[test]
fn debug_config_emits_only_resolved_json() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = rust_binary();
    command.args(["debug", "config"]);
    isolated_subject(&mut command, root.path());
    command.env(
        "OPENCODE_CONFIG_CONTENT",
        r#"{"username":"debug-user","share":"disabled","plugin":["probe-plugin@1.0.0"]}"#,
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
    assert_eq!(config["plugin"], serde_json::json!(["probe-plugin@1.0.0"]));
    assert_eq!(
        config["plugin_origins"],
        serde_json::json!([{
            "spec": "probe-plugin@1.0.0",
            "source": "OPENCODE_CONFIG_CONTENT",
            "scope": "local"
        }])
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

    let mut command = rust_binary();
    command.args(["debug", "config"]);
    isolated_subject(&mut command, root.path());
    command.env("ZUNO_PURE", "1");
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

#[test]
fn criterion_2_pure_debug_config_matches_the_released_binary() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let root = tempfile::tempdir().expect("isolated root");
    let run = |binary: &Path| {
        let stdout = tempfile::NamedTempFile::new().expect("stdout capture");
        let stderr = tempfile::NamedTempFile::new().expect("stderr capture");
        let mut command = Command::new(binary);
        command
            .args(["debug", "config"])
            .current_dir(&workspace)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("USER", "user")
            .stdout(stdout.reopen().expect("reopen stdout"))
            .stderr(stderr.reopen().expect("reopen stderr"));
        isolated_for_binary(&mut command, binary, root.path());
        if binary == rust_path() {
            command.env("ZUNO_PURE", "1");
        } else {
            command.env("OPENCODE_PURE", "1");
        }
        let status = command.status().expect("debug config");
        let stdout = std::fs::read(stdout.path()).expect("read stdout");
        let stderr = std::fs::read(stderr.path()).expect("read stderr");
        (status, stdout, stderr)
    };
    let rust = run(&rust_path());
    // Run the hard-cut subject first. On an empty tree the released oracle creates
    // its legacy default config, which the subject must correctly diagnose as an
    // unmigrated install rather than silently treating as its own input.
    let released = run(oracle());
    assert!(
        released.0.success(),
        "released debug config failed: {}",
        String::from_utf8_lossy(&released.2)
    );
    assert!(
        rust.0.success(),
        "Rust debug config failed: {}",
        String::from_utf8_lossy(&rust.2)
    );

    let mut expected: serde_json::Value =
        serde_json::from_slice(&released.1).expect("released debug config JSON");
    let mode = expected
        .as_object_mut()
        .expect("released config object")
        .remove("mode");
    assert_eq!(
        mode,
        Some(serde_json::json!({})),
        "only the released binary's empty deprecated mode object is excluded"
    );
    let actual: serde_json::Value =
        serde_json::from_slice(&rust.1).expect("Rust debug config JSON");
    assert_eq!(actual, expected);
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
    let mut command = Command::new(oracle());
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
    isolated_oracle(&mut command, root);
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
    isolated_subject(&mut command, root);
    command.env("ZUNO_DB", database);
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
    if pinned_oracle_or_skip(
        "session_list_all_projects_matches_the_experimental_endpoint_on_one_database",
        "the cross-project listing was NOT compared against /experimental/session",
    )
    .is_none()
    {
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

// ---------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------

const EXPORT_SESSION: &str = "ses_diffexport0000000000000000ab";
const EXPORT_USER: &str = "msg_diffexport0000000000000000us";
const EXPORT_ASSISTANT: &str = "msg_diffexport0000000000000000as";

/// Seed one project, one session, two messages, and one part of every variant.
///
/// Every variant is present because `export` is what decodes the two opaque
/// `data` blobs through a schema: a fixture carrying only `text` parts would let
/// a whole family of payloads diverge from the oracle unobserved. The payloads
/// are the ones `oc-db/tests/message_export.rs` already proved the real binary
/// accepts, so a failure here is about the *envelope*, not about the blobs.
fn seed_export_database(path: &Path) {
    use oc_db::message::{MessageRecord, MessageStore, PartKind, PartRecord};

    std::fs::create_dir_all(path.parent().expect("database has a parent"))
        .expect("create the data directory");
    let mut connection = oc_db::open::open_at(path).expect("create the export database");
    oc_db::migration::apply(&mut connection).expect("apply the schema");
    connection
        .execute_batch(&format!(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('prj_diffexport', '/srv/diffexport', 1780034795000, 1780034795000, '[]');
             INSERT INTO session \
               (id, project_id, slug, directory, path, title, version, cost, tokens_input, \
                tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, agent, \
                model, metadata, time_created, time_updated) \
             VALUES ('{EXPORT_SESSION}', 'prj_diffexport', 'diff-export', '/srv/diffexport', '', \
                     'a differential export', '1.18.13', 2.0, 10, 20, 0, 1, 2, 'build', \
                     '{{\"id\":\"claude-sonnet-4-5\",\"providerID\":\"anthropic\"}}', \
                     '{{\"ticket\":\"OC-116\"}}', 1780034795000, 1780034796000);"
        ))
        .expect("seed a project and a session");

    let store = MessageStore::new(&connection);
    let user = MessageRecord::from_json(serde_json::json!({
        "id": EXPORT_USER,
        "sessionID": EXPORT_SESSION,
        "role": "user",
        "time": { "created": 1_780_034_795_100_i64 },
        "agent": "build",
        "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" },
    }))
    .expect("split the user message");
    store
        .put_message_at(&user, 1_780_034_795_100)
        .expect("write the user message");

    let assistant = MessageRecord::from_json(serde_json::json!({
        "id": EXPORT_ASSISTANT,
        "sessionID": EXPORT_SESSION,
        "role": "assistant",
        "time": { "created": 1_780_034_795_200_i64, "completed": 1_780_034_796_000_i64 },
        "parentID": EXPORT_USER,
        "modelID": "claude-sonnet-4-5",
        "providerID": "anthropic",
        "mode": "build",
        "agent": "build",
        "path": { "cwd": "/srv/diffexport", "root": "/srv/diffexport" },
        "cost": 0.004_25,
        "tokens": {
            "input": 1_024.0,
            "output": 256.0,
            "reasoning": 0.0,
            "cache": { "read": 0.0, "write": 0.0 },
        },
    }))
    .expect("split the assistant message");
    store
        .put_message_at(&assistant, 1_780_034_795_200)
        .expect("write the assistant message");

    for (index, kind) in PartKind::ALL.into_iter().enumerate() {
        let part_id = format!("prt_diffexport000000000000000{index:03}");
        let mut payload = export_part_payload(kind);
        let object = payload.as_object_mut().expect("payload is an object");
        object.insert("id".to_owned(), serde_json::json!(part_id));
        object.insert("sessionID".to_owned(), serde_json::json!(EXPORT_SESSION));
        object.insert("messageID".to_owned(), serde_json::json!(EXPORT_ASSISTANT));
        let created = 1_780_034_795_300 + i64::try_from(index).expect("index fits");
        let record = PartRecord::from_json(payload, created)
            .unwrap_or_else(|error| panic!("{kind}: split: {error}"));
        store
            .put_part_at(&record, created)
            .unwrap_or_else(|error| panic!("{kind}: write: {error}"));
    }
}

fn export_part_payload(kind: oc_db::message::PartKind) -> serde_json::Value {
    use oc_db::message::PartKind;
    use serde_json::json;

    match kind {
        PartKind::Text => json!({ "type": "text", "text": "hello from rust" }),
        PartKind::Reasoning => json!({
            "type": "reasoning",
            "text": "weighing the index order",
            "time": { "start": 1_780_034_795_239_i64, "end": 1_780_034_795_999_i64 },
        }),
        PartKind::Tool => json!({
            "type": "tool",
            "callID": "toolu_01RUST",
            "tool": "read",
            "state": {
                "status": "completed",
                "input": { "filePath": "/workspace/src/lib.rs" },
                "output": "pub mod message;",
                "title": "src/lib.rs",
                "metadata": { "lines": 1 },
                "time": { "start": 1_780_034_795_239_i64, "end": 1_780_034_795_512_i64 },
            },
        }),
        PartKind::StepStart => json!({ "type": "step-start" }),
        PartKind::StepFinish => json!({
            "type": "step-finish",
            "reason": "stop",
            "cost": 0.001_25,
            "tokens": {
                "input": 100.0,
                "output": 20.0,
                "reasoning": 0.0,
                "cache": { "read": 0.0, "write": 0.0 },
            },
        }),
        PartKind::Patch => json!({
            "type": "patch",
            "hash": "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00",
            "files": ["crates/oc-db/src/message.rs"],
        }),
        PartKind::File => json!({
            "type": "file",
            "mime": "text/plain",
            "filename": "note.txt",
            "url": "data:text/plain;base64,aGVsbG8=",
        }),
        PartKind::Compaction => json!({ "type": "compaction", "auto": false }),
        PartKind::Subtask => json!({
            "type": "subtask",
            "prompt": "audit the parser",
            "description": "parser audit",
            "agent": "explore",
        }),
        PartKind::Snapshot => json!({
            "type": "snapshot",
            "snapshot": "9f2c1ab0d3e4f5061728394a5b6c7d8e9f001122",
        }),
        PartKind::Agent => json!({ "type": "agent", "name": "explore" }),
        PartKind::Retry => json!({
            "type": "retry",
            "attempt": 1,
            "error": {
                "name": "APIError",
                "data": { "message": "overloaded", "isRetryable": true },
            },
            "time": { "created": 1_780_034_795_239_i64 },
        }),
    }
}

/// Run one binary's `export` against `database` and parse its stdout.
fn export_document(
    binary: &Path,
    root: &Path,
    database: &Path,
    args: &[&str],
) -> serde_json::Value {
    let mut command = Command::new(binary);
    command.args(args);
    isolated_for_binary(&mut command, binary, root);
    if binary == rust_path() {
        command.env("ZUNO_DB", database);
    } else {
        command.env("OPENCODE_DB", database);
    }
    let output = command.output().expect("run export");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "{} export exited with {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        binary.display(),
        output.status,
    );
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("export did not print JSON ({error}):\n{stdout}"))
}

/// The single acceptance test for todo 116: a seeded session exports with exit 0
/// and a payload the real binary also produces.
///
/// This is the reason the check is a differential rather than a shape assertion.
/// `export` was already *documented* as implemented and *recorded* as implemented
/// while exiting 1, so any test that consults this port's own description of
/// itself would have passed then too. Comparing against the released binary's
/// bytes is the only assertion that cannot agree with a mistake in this repo.
#[test]
fn export_matches_the_oracle_on_one_seeded_session() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("data").join("zuno").join("export.db");
    seed_export_database(&database);

    let actual = export_document(
        &rust_path(),
        root.path(),
        &database,
        &["export", EXPORT_SESSION],
    );

    let messages = actual
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .expect("the export carries a messages array");
    assert_eq!(messages.len(), 2, "{actual}");
    assert_eq!(
        actual
            .pointer("/info/id")
            .and_then(serde_json::Value::as_str),
        Some(EXPORT_SESSION)
    );
    assert_eq!(
        messages[1]
            .pointer("/parts")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(oc_db::message::PART_KIND_COUNT),
        "every seeded part variant must survive the export"
    );

    let Some(oracle_program) = pinned_oracle_or_skip(
        "export_matches_the_oracle_on_one_seeded_session",
        "the export payload was NOT compared against a real release",
    ) else {
        return;
    };
    let oracle = export_document(
        oracle_program,
        root.path(),
        &database,
        &["export", EXPORT_SESSION, "--pure"],
    );
    assert_eq!(
        actual,
        oracle,
        "export payloads differ\nRust:\n{}\nOracle:\n{}",
        serde_json::to_string_pretty(&actual).expect("render"),
        serde_json::to_string_pretty(&oracle).expect("render"),
    );
}

#[test]
fn export_sanitize_matches_the_oracle_on_one_seeded_session() {
    let Some(oracle_program) = pinned_oracle_or_skip(
        "export_sanitize_matches_the_oracle_on_one_seeded_session",
        "the redaction pass was NOT compared against a real release",
    ) else {
        return;
    };
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("data").join("zuno").join("export.db");
    seed_export_database(&database);

    let actual = export_document(
        &rust_path(),
        root.path(),
        &database,
        &["export", EXPORT_SESSION, "--sanitize"],
    );
    let oracle = export_document(
        oracle_program,
        root.path(),
        &database,
        &["export", EXPORT_SESSION, "--sanitize", "--pure"],
    );
    assert_eq!(
        actual,
        oracle,
        "redacted export payloads differ\nRust:\n{}\nOracle:\n{}",
        serde_json::to_string_pretty(&actual).expect("render"),
        serde_json::to_string_pretty(&oracle).expect("render"),
    );
}

/// `export | import` restores the transcript into a second database.
///
/// The assertion is on the *re-export* rather than on row counts, because a row
/// count cannot tell a restored part from an empty one.
#[test]
fn export_then_import_restores_the_session_into_another_database() {
    let root = tempfile::tempdir().expect("tempdir");
    let source = root.path().join("data").join("zuno").join("source.db");
    seed_export_database(&source);
    let exported = export_document(
        &rust_path(),
        root.path(),
        &source,
        &["export", EXPORT_SESSION],
    );

    let file = root.path().join("exported.json");
    std::fs::write(
        &file,
        serde_json::to_string_pretty(&exported).expect("render"),
    )
    .expect("write the export");

    let target = root.path().join("data").join("zuno").join("target.db");
    let mut command = rust_binary();
    command.args(["import", &file.to_string_lossy()]);
    isolated_subject(&mut command, root.path());
    command.env("ZUNO_DB", &target).current_dir(root.path());
    let output = command.output().expect("run import");
    assert!(
        output.status.success(),
        "import exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(EXPORT_SESSION),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let restored = export_document(
        &rust_path(),
        root.path(),
        &target,
        &["export", EXPORT_SESSION],
    );
    // `directory` and `path` are re-homed onto the importing checkout by design
    // (`cli/cmd/import.ts:178-183`); everything else must come back verbatim.
    let mut expected = exported;
    let info = expected
        .get_mut("info")
        .and_then(serde_json::Value::as_object_mut)
        .expect("info");
    info.insert(
        "directory".to_owned(),
        restored
            .pointer("/info/directory")
            .cloned()
            .expect("a restored directory"),
    );
    info.insert(
        "projectID".to_owned(),
        restored
            .pointer("/info/projectID")
            .cloned()
            .expect("a restored projectID"),
    );
    if let Some(path) = restored.pointer("/info/path").cloned() {
        info.insert("path".to_owned(), path);
    } else {
        info.remove("path");
    }
    assert_eq!(
        restored,
        expected,
        "the imported session did not round-trip\nrestored:\n{}\nexpected:\n{}",
        serde_json::to_string_pretty(&restored).expect("render"),
        serde_json::to_string_pretty(&expected).expect("render"),
    );
}

#[test]
fn export_without_a_session_id_explains_the_interactive_selection() {
    let root = tempfile::tempdir().expect("tempdir");
    let output = run(&rust_path(), &["export"], root.path());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interactive"), "{stderr}");
    assert!(
        !stderr.contains("pending"),
        "a missing argument must not report a missing handler: {stderr}"
    );
}

#[test]
fn export_reports_an_unknown_session_the_way_the_oracle_does() {
    let root = tempfile::tempdir().expect("tempdir");
    let database = root.path().join("data").join("zuno").join("export.db");
    seed_export_database(&database);
    let missing = "ses_diffexport0000000000000000zz";

    let mut command = rust_binary();
    command.args(["export", missing]);
    isolated_subject(&mut command, root.path());
    command.env("ZUNO_DB", &database);
    let output = command.output().expect("run export");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("Session not found: {missing}")),
        "{stderr}"
    );
}

#[test]
fn import_reports_a_missing_file_the_way_the_oracle_does() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut command = rust_binary();
    command.args(["import", "/definitely/absent/export.json"]);
    isolated_subject(&mut command, root.path());
    command.current_dir(root.path());
    let output = command.output().expect("run import");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("File not found"), "{stderr}");
}
