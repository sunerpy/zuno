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
    command.env(
        "ZUNO_CONFIG_CONTENT",
        r#"{"provider":{"anyapi":{"surface":"responses"}}}"#,
    );
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
    assert!(
        stdout.contains("\"endpoint\": \"responses\""),
        "the resolved API surface is not inspectable:\n{stdout}"
    );
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
    custom.env(
        "ZUNO_CONFIG_CONTENT",
        r#"{
          "provider": {
            "myopenai": {
              "transport": "openai",
              "models": {"gpt-test": {"name": "GPT Test"}}
            }
          }
        }"#,
    );
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
fn plugin_add_update_list_and_remove_use_project_extension_storage() {
    let root = tempfile::tempdir().expect("tempdir");
    let source = root.path().join("plugin-source");
    std::fs::create_dir(&source).expect("plugin source");
    let manifest = source.join("extension.json");
    std::fs::write(
        &manifest,
        r#"{
  "apiVersion": "zuno.extension/v1",
  "id": "cli-plugin",
  "description": "first plugin revision",
  "agents": {
    "network-reviewer": {
      "description": "Reviews files and network evidence.",
      "mode": "subagent",
      "prompt": "Use native tools to review the request.",
      "permission": {
        "mode": "standard",
        "rules": {
          "read": "allow",
          "glob": "allow",
          "grep": "allow",
          "web_search": "allow",
          "shell": "ask"
        }
      }
    }
  },
  "workflows": {
    "network-review": {
      "description": "Delegate a network-aware review.",
      "prompt": "Use task with agent=network-reviewer and a complete typed delegation contract for: $ARGUMENTS"
    }
  }
}"#,
    )
    .expect("first plugin manifest");
    let source_text = source.to_str().expect("UTF-8 source");
    let root_text = root.path().to_str().expect("UTF-8 root");

    let added = run(
        root.path(),
        &[
            "plugin",
            "add",
            source_text,
            "--project",
            "--dir",
            root_text,
        ],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(
        root.path()
            .join(".zuno/extensions/cli-plugin/extension.json")
            .is_file()
    );

    let listed = run(root.path(), &["plugin", "list", "--dir", root_text]);
    assert!(listed.status.success());
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("cli-plugin (declarative)"), "{stdout}");
    assert!(stdout.contains("agents: network-reviewer"), "{stdout}");
    assert!(stdout.contains("workflows: network-review"), "{stdout}");

    let updated_manifest = std::fs::read_to_string(&manifest)
        .expect("read manifest")
        .replace("first plugin revision", "second plugin revision");
    std::fs::write(&manifest, updated_manifest).expect("second plugin manifest");
    let updated = run(
        root.path(),
        &[
            "plugin",
            "update",
            source_text,
            "--project",
            "--dir",
            root_text,
        ],
    );
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let listed = run(root.path(), &["plugin", "list", "--dir", root_text]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("second plugin revision"),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );

    let removed = run(
        root.path(),
        &[
            "plugin",
            "remove",
            "cli-plugin",
            "--project",
            "--dir",
            root_text,
        ],
    );
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let listed = run(root.path(), &["plugin", "list", "--dir", root_text]);
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("No plugins active"),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
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
            // `model` and `small_model` stand in for the scalar keys this test used
            // to carry: `username` and `share` were removed because nothing read
            // them, so a config naming either one is now a validation error.
            r#"{"model":"debug/primary","small_model":"debug/fast","web_search":{"provider":"exa","max_queries":3,"max_results":7,"timeout_ms":12000}}"#,
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
    assert_eq!(config["model"], "debug/primary");
    assert_eq!(config["small_model"], "debug/fast");
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

fn seed_portable_configuration(root: &Path) {
    let config = root.join("config/zuno");
    let home = root.join("home/.zuno");
    let data = root.join("data/zuno");
    let cache = root.join("cache/zuno");
    for directory in [
        config.join("skill/release-helper/references"),
        config.join("extensions/example"),
        config.join("agent"),
        config.join("command"),
        config.join("profiles/kiro"),
        home.join("skill/home-helper"),
        data.clone(),
        cache.clone(),
    ] {
        std::fs::create_dir_all(directory).expect("portable fixture directory");
    }
    std::fs::write(config.join("AGENTS.md"), "# Global Zuno rules\n").expect("global AGENTS");
    std::fs::write(config.join("zuno.json"), r#"{"provider":{"local":{}}}"#)
        .expect("global config");
    std::fs::write(
        config.join("skill/release-helper/SKILL.md"),
        "# Release helper\n",
    )
    .expect("skill");
    std::fs::write(
        config.join("skill/release-helper/references/checklist.md"),
        "# Checklist\n",
    )
    .expect("skill reference");
    std::fs::write(
        config.join("extensions/example/extension.json"),
        r#"{"id":"example","runtime":{"kind":"process","command":["example"]}}"#,
    )
    .expect("extension");
    std::fs::write(config.join("agent/reviewer.md"), "# Reviewer\n").expect("agent");
    std::fs::write(config.join("command/check.md"), "# Check\n").expect("command");
    std::fs::write(
        config.join("profiles/kiro/zuno.json"),
        r#"{"provider":{"kiro":{}}}"#,
    )
    .expect("profile");
    std::fs::write(home.join("skill/home-helper/SKILL.md"), "# Home helper\n").expect("home skill");
    std::fs::write(data.join("auth.json"), r#"{"local":{"key":"secret"}}"#).expect("credentials");
    std::fs::write(data.join("zuno-local.db"), b"session database").expect("database");
    std::fs::write(cache.join("models.json"), b"cache").expect("cache");
}

fn export_bundle(root: &Path, bundle: &Path, include_credentials: bool) -> Output {
    let mut command = zuno();
    command.arg("export").arg(bundle).current_dir(root);
    if include_credentials {
        command.arg("--include-credentials");
    }
    isolated(&mut command, root);
    command.output().expect("run portable export")
}

#[test]
fn export_then_import_restores_portable_configuration_without_runtime_state() {
    let source = tempfile::tempdir().expect("source");
    seed_portable_configuration(source.path());
    let bundle = source.path().join("portable.zuno-bundle");
    let output = export_bundle(source.path(), &bundle, false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(bundle.is_file());

    let target = tempfile::tempdir().expect("target");
    let mut command = zuno();
    command
        .arg("import")
        .arg(&bundle)
        .current_dir(target.path());
    isolated(&mut command, target.path());
    let output = command.output().expect("run portable import");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(target.path().join("config/zuno/AGENTS.md"))
            .expect("imported AGENTS"),
        "# Global Zuno rules\n"
    );
    assert!(target.path().join("config/zuno/zuno.json").is_file());
    assert!(
        target
            .path()
            .join("config/zuno/skill/release-helper/references/checklist.md")
            .is_file()
    );
    assert!(
        target
            .path()
            .join("config/zuno/extensions/example/extension.json")
            .is_file()
    );
    assert!(
        target
            .path()
            .join("home/.zuno/skill/home-helper/SKILL.md")
            .is_file()
    );
    assert!(!target.path().join("data/zuno/auth.json").exists());
    assert!(!target.path().join("data/zuno/zuno-local.db").exists());
    assert!(!target.path().join("cache/zuno/models.json").exists());
}

#[test]
fn export_without_a_path_creates_a_named_portable_bundle() {
    let root = tempfile::tempdir().expect("tempdir");
    seed_portable_configuration(root.path());
    let output = run(root.path(), &["export"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bundles = std::fs::read_dir(root.path())
        .expect("export directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("zuno-export-") && name.ends_with(".zuno-bundle"))
        .collect::<Vec<_>>();
    assert_eq!(bundles.len(), 1, "{bundles:?}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("session"));
}

#[test]
fn import_requires_explicit_replace_and_credentials_are_opt_in() {
    let source = tempfile::tempdir().expect("source");
    seed_portable_configuration(source.path());
    let bundle = source.path().join("portable-with-credentials.zuno-bundle");
    let output = export_bundle(source.path(), &bundle, true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unencrypted credentials"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target = tempfile::tempdir().expect("target");
    let existing = target.path().join("config/zuno");
    std::fs::create_dir_all(&existing).expect("target config");
    std::fs::write(existing.join("zuno.json"), r#"{"keep":true}"#).expect("target marker");

    let mut command = zuno();
    command
        .arg("import")
        .arg(&bundle)
        .current_dir(target.path());
    isolated(&mut command, target.path());
    let output = command.output().expect("import without replace");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--replace"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(existing.join("zuno.json")).expect("unchanged target"),
        r#"{"keep":true}"#
    );

    let mut command = zuno();
    command
        .arg("import")
        .arg(&bundle)
        .arg("--replace")
        .current_dir(target.path());
    isolated(&mut command, target.path());
    let output = command.output().expect("import with replace");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(target.path().join("data/zuno/auth.json"))
            .expect("imported credentials"),
        r#"{"local":{"key":"secret"}}"#
    );
    assert!(!target.path().join("data/zuno/zuno-local.db").exists());
}

#[test]
fn an_opencode_named_file_is_not_part_of_the_zuno_config_graph() {
    let root = tempfile::tempdir().expect("tempdir");
    let unrelated = root.path().join("config/zuno/opencode.json");
    std::fs::create_dir_all(unrelated.parent().expect("parent")).expect("config dir");
    std::fs::write(&unrelated, r#"{"model":"probe/not-zuno-input"}"#).expect("seed file");

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
    std::fs::write(&config, r#"{"model":"probe/canonical-name-reader"}"#).expect("seed config");

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
