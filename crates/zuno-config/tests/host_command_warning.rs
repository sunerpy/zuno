//! A project-layer config that names a host executable is a warning, not a rejection.
//!
//! The warning is only observable through the process-wide log sink, so this file owns
//! the subscriber for its own test binary and reads the plaintext log back.

use std::fs;

use zuno_config::discovery::{DiscoveryOptions, discover_with};
use zuno_config::schema::mcp::McpServerConfig;
use zuno_observability::{LogConfig, LogLevel};
use zuno_paths::Env;

#[test]
fn project_layer_host_commands_are_warned_about_but_not_rejected() {
    // Given: a checkout whose project config declares five kinds of host command.
    let temp = tempfile::tempdir().expect("temporary discovery root");
    let root = temp.path();
    let home = root.join("home");
    let xdg_config = root.join("xdg-config");
    let project = root.join("project");
    for directory in [
        &home,
        &xdg_config,
        &project.join(".git"),
        &project.join(".zuno"),
    ] {
        fs::create_dir_all(directory).expect("create fixture directory");
    }
    fs::write(
        project.join(".zuno/zuno.json"),
        r#"{
            "shell": "./bin/project-shell",
            "mcp": {
                "repo-tool": {"type": "local", "command": ["./scripts/mcp.sh"]},
                "docs": {"type": "remote", "url": "https://example.invalid/mcp"}
            },
            "lsp": {"custom": {"command": ["./bin/lsp"], "extensions": [".zx"]}},
            "formatter": {"fmt": {"command": ["./bin/fmt", "$FILE"]}},
            "productAgent": {"helper": {"kind": "codex", "command": "./bin/codex"}}
        }"#,
    )
    .expect("write project config");
    let handle = zuno_observability::init(
        LogConfig::from_env(root.join("logs"))
            .with_level(LogLevel::Warn)
            .with_plaintext_logs(true)
            .with_print_logs(false),
    )
    .expect("logging initializes");
    assert!(
        handle.installed(),
        "this test binary owns the log subscriber"
    );

    // When: discovery merges the project layer.
    let options = DiscoveryOptions::new(
        &project,
        Some(project.clone()),
        Env::from_pairs([
            ("HOME".to_owned(), home.display().to_string()),
            ("ZUNO_TEST_HOME".to_owned(), home.display().to_string()),
            (
                "XDG_CONFIG_HOME".to_owned(),
                xdg_config.display().to_string(),
            ),
        ]),
    )
    .with_default_username("unknown");
    let config = discover_with(&options)
        .expect("host-command declarations in a project layer are warnings, not rejections");

    // Then: the declarations are kept verbatim...
    assert_eq!(config.shell.as_deref(), Some("./bin/project-shell"));
    assert!(matches!(
        config.mcp.as_ref().and_then(|mcp| mcp.get("repo-tool")),
        Some(McpServerConfig::Local(_))
    ));

    // ...and every host command is named in a warning the operator can act on.
    let plaintext = handle
        .plaintext_path()
        .expect("plaintext log path")
        .to_path_buf();
    drop(handle);
    let log = fs::read_to_string(&plaintext).expect("read plaintext log");
    for key in [
        "shell",
        "mcp.repo-tool.command",
        "lsp.custom.command",
        "formatter.fmt.command",
        "productAgent.helper.command",
    ] {
        assert!(
            log.lines()
                .any(|line| line.contains("WARN") && line.contains(key)),
            "expected a warning naming `{key}` in the log:\n{log}"
        );
    }
    assert!(
        !log.contains("mcp.docs"),
        "a remote MCP server runs nothing on this machine and must not be warned about:\n{log}"
    );
}
