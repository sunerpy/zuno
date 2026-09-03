//! A project-layer config that names a host executable is refused until a trusted
//! layer admits the checkout.
//!
//! Trust is granted by `trust.project_host_commands`, which only a trusted layer can
//! set. The admitted case is only observable through the process-wide log sink, so
//! this file owns the subscriber for its own test binary and reads the plaintext log
//! back.

use std::fs;
use std::path::PathBuf;

use zuno_config::discovery::{DiscoveryOptions, discover_with};
use zuno_config::schema::mcp::McpServerConfig;
use zuno_error::ConfigError;
use zuno_observability::{LogConfig, LogLevel};
use zuno_paths::Env;

/// Every kind of project-layer entry that would have this host run a command, plus
/// one remote MCP server that runs nothing locally.
const PROJECT_HOST_COMMANDS: &str = r#"{
    "shell": "./bin/project-shell",
    "mcp": {
        "repo-tool": {"type": "local", "command": ["./scripts/mcp.sh"]},
        "docs": {"type": "remote", "url": "https://example.invalid/mcp"}
    },
    "lsp": {"custom": {"command": ["./bin/lsp"], "extensions": [".zx"]}},
    "formatter": {"fmt": {"command": ["./bin/fmt", "$FILE"]}},
    "productAgent": {"helper": {"kind": "codex", "command": "./bin/codex"}}
}"#;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
}

impl Fixture {
    /// A checkout at `<root>/project` whose `.zuno/zuno.json` holds `project`.
    fn new(project_config: &str) -> Self {
        let temp = tempfile::tempdir().expect("temporary discovery root");
        let root = temp.path().to_path_buf();
        let project = root.join("project");
        for directory in [
            &root.join("home"),
            &root.join("xdg-config"),
            &project.join(".git"),
            &project.join(".zuno"),
        ] {
            fs::create_dir_all(directory).expect("create fixture directory");
        }
        fs::write(project.join(".zuno/zuno.json"), project_config).expect("write project config");
        Self {
            _temp: temp,
            root,
            project,
        }
    }

    /// Write the trusted global layer, the one layer a project cannot author.
    fn with_trusted_global(self, contents: &str) -> Self {
        let path = self.root.join("xdg-config/zuno/zuno.json");
        fs::create_dir_all(path.parent().expect("global config parent"))
            .expect("create global config directory");
        fs::write(path, contents).expect("write trusted global config");
        self
    }

    fn project_config(&self) -> PathBuf {
        self.project.join(".zuno/zuno.json")
    }

    fn options(&self) -> DiscoveryOptions {
        DiscoveryOptions::new(
            &self.project,
            Some(self.project.clone()),
            Env::from_pairs([
                (
                    "HOME".to_owned(),
                    self.root.join("home").display().to_string(),
                ),
                (
                    "ZUNO_TEST_HOME".to_owned(),
                    self.root.join("home").display().to_string(),
                ),
                (
                    "XDG_CONFIG_HOME".to_owned(),
                    self.root.join("xdg-config").display().to_string(),
                ),
            ]),
        )
        .with_default_username("unknown")
    }
}

/// A trusted global layer granting host commands to exactly these roots.
fn trust_roots(roots: &[&PathBuf]) -> String {
    let roots: Vec<String> = roots
        .iter()
        .map(|root| root.display().to_string())
        .collect();
    serde_json::json!({"trust": {"project_host_commands": roots}}).to_string()
}

fn refusal(fixture: &Fixture) -> (PathBuf, Vec<zuno_error::ConfigIssue>) {
    match discover_with(&fixture.options()) {
        Err(ConfigError::Invalid { path, issues }) => (path, issues),
        Err(other) => panic!("expected a validation refusal, got {other:?}"),
        Ok(_) => panic!("expected discovery to refuse the project layer"),
    }
}

fn keys(issues: &[zuno_error::ConfigIssue]) -> Vec<String> {
    issues
        .iter()
        .map(|issue| issue.key_path.join("."))
        .collect()
}

#[test]
fn a_project_layer_host_command_is_refused_without_explicit_trust() {
    // Given: a checkout that declares five kinds of host command and no trust anywhere.
    let fixture = Fixture::new(PROJECT_HOST_COMMANDS);

    // When: discovery merges the project layer.
    let (path, issues) = refusal(&fixture);

    // Then: the refusal names the exact file and every command key in it.
    assert_eq!(path, fixture.project_config());
    assert_eq!(
        keys(&issues),
        [
            "shell",
            "mcp.repo-tool.command",
            "lsp.custom.command",
            "formatter.fmt.command",
            "productAgent.helper.command",
        ]
    );
    for issue in &issues {
        assert!(
            issue.detail.contains("trust.project_host_commands"),
            "a refusal must name its upgrade path: {}",
            issue.detail
        );
    }
    assert!(
        !keys(&issues).iter().any(|key| key.contains("mcp.docs")),
        "a remote MCP server runs nothing on this machine: {:?}",
        keys(&issues)
    );
}

#[test]
fn a_bare_project_config_file_is_refused_like_a_zuno_directory_layer() {
    // Given: the root `zuno.json` of a checkout, which is a project layer too.
    let fixture = Fixture::new("{}");
    let bare = fixture.project.join("zuno.json");
    fs::write(
        &bare,
        r#"{"mcp":{"repo-tool":{"type":"local","command":["./x"]}}}"#,
    )
    .expect("write bare project config");

    // When/Then: discovery refuses it and names that file, not the `.zuno` one.
    let (path, issues) = refusal(&fixture);
    assert_eq!(path, bare);
    assert_eq!(keys(&issues), ["mcp.repo-tool.command"]);
}

#[test]
fn a_project_layer_cannot_grant_itself_trust() {
    // Given: a checkout that tries to trust itself, declaring no command at all.
    let fixture = Fixture::new(r#"{"trust":{"project_host_commands":true}}"#);

    // When/Then: the trust key alone is the refusal, so no merged trust value can
    // ever have come from a checkout.
    let (path, issues) = refusal(&fixture);
    assert_eq!(path, fixture.project_config());
    assert_eq!(keys(&issues), ["trust"]);
    assert!(
        issues[0].detail.contains("cannot grant itself trust"),
        "{}",
        issues[0].detail
    );
}

#[test]
fn a_trusted_root_beside_the_checkout_does_not_admit_it() {
    // Given: trust for a sibling directory only.
    let fixture = Fixture::new(PROJECT_HOST_COMMANDS);
    let sibling = fixture.root.join("other-project");
    fs::create_dir_all(&sibling).expect("create sibling project");
    let grant = trust_roots(&[&sibling]);
    let fixture = fixture.with_trusted_global(&grant);

    // When/Then: the neighbouring grant does not reach this checkout.
    let (path, _) = refusal(&fixture);
    assert_eq!(path, fixture.project_config());
}

#[test]
fn a_relative_trusted_root_is_refused_at_the_layer_that_wrote_it() {
    // Given: a trusted global layer with a root that depends on the current directory.
    let fixture = Fixture::new("{}")
        .with_trusted_global(r#"{"trust":{"project_host_commands":["./checkouts"]}}"#);

    // When/Then: the trusted layer itself fails validation, rather than silently
    // trusting nothing.
    let error =
        discover_with(&fixture.options()).expect_err("a relative trust root is not a decision");
    let ConfigError::Invalid { path, issues } = error else {
        panic!("expected the relative root to fail validation");
    };
    assert_eq!(path, fixture.root.join("xdg-config/zuno/zuno.json"));
    assert!(
        issues
            .iter()
            .any(|issue| issue.detail.contains("absolute path")),
        "{issues:?}"
    );
}

#[test]
fn a_trusted_project_root_admits_the_checkouts_host_commands() {
    // Given: the trusted global layer names this checkout's parent directory.
    let fixture = Fixture::new(PROJECT_HOST_COMMANDS);
    let grant = trust_roots(&[&fixture.root]);
    let fixture = fixture.with_trusted_global(&grant);
    let handle = zuno_observability::init(
        LogConfig::from_env(fixture.root.join("logs"))
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
    let config = discover_with(&fixture.options())
        .expect("an explicitly trusted checkout may declare host commands");

    // Then: the declarations are kept verbatim...
    assert_eq!(config.shell.as_deref(), Some("./bin/project-shell"));
    assert!(matches!(
        config.mcp.as_ref().and_then(|mcp| mcp.get("repo-tool")),
        Some(McpServerConfig::Local(_))
    ));

    // ...and every admitted command is still named in a warning, because the
    // checkout made the choice and the operator should be able to see it.
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
        "a remote MCP server runs nothing on this machine:\n{log}"
    );
}

#[test]
fn trusting_every_checkout_admits_this_one() {
    // Given: a host that has decided every checkout may declare host commands.
    let fixture = Fixture::new(PROJECT_HOST_COMMANDS)
        .with_trusted_global(r#"{"trust":{"project_host_commands":true}}"#);

    // When/Then: discovery keeps the declarations.
    let config = discover_with(&fixture.options()).expect("blanket trust admits every checkout");
    assert_eq!(config.shell.as_deref(), Some("./bin/project-shell"));
}

#[test]
fn refusing_every_checkout_is_the_same_as_saying_nothing() {
    // Given: an explicit `false`, which is the default written down.
    let fixture = Fixture::new(PROJECT_HOST_COMMANDS)
        .with_trusted_global(r#"{"trust":{"project_host_commands":false}}"#);

    // When/Then: the refusal is unchanged.
    let (path, issues) = refusal(&fixture);
    assert_eq!(path, fixture.project_config());
    assert!(!issues.is_empty());
}

#[test]
fn a_trusted_layer_may_declare_host_commands_without_any_trust_grant() {
    // Given: the same declarations in the trusted global layer instead of the checkout.
    let fixture = Fixture::new("{}").with_trusted_global(PROJECT_HOST_COMMANDS);

    // When/Then: trust is about checkouts, so this host's own config needs no grant.
    let config =
        discover_with(&fixture.options()).expect("a trusted layer already speaks for this host");
    assert_eq!(config.shell.as_deref(), Some("./bin/project-shell"));
}

/// An off switch beside the command is not evidence the program will not run: it
/// lives in the same untrusted layer as the command, and any later layer can flip it
/// without restating the command. Every section is refused on the declaration alone,
/// so which dormant executable is tolerated does not depend on which section holds
/// it.
#[test]
fn a_dormant_project_layer_host_command_is_refused_like_an_active_one() {
    // Given: a checkout where every host command it declares is switched off — the
    // local MCP server and the LSP and formatter entries explicitly, and the product
    // agent by the default that leaves it disabled unless `enabled` is true.
    let fixture = Fixture::new(
        r#"{
    "mcp": {"repo-tool": {"type": "local", "command": ["./scripts/mcp.sh"], "enabled": false}},
    "lsp": {"custom": {"command": ["./bin/lsp"], "extensions": [".zx"], "disabled": true}},
    "formatter": {"fmt": {"command": ["./bin/fmt", "$FILE"], "disabled": true}},
    "productAgent": {"helper": {"kind": "codex", "command": "./bin/helper"}}
}"#,
    );

    // When/Then: the refusal still names every command key in the file.
    let (path, issues) = refusal(&fixture);
    assert_eq!(path, fixture.project_config());
    assert_eq!(
        keys(&issues),
        [
            "mcp.repo-tool.command",
            "lsp.custom.command",
            "formatter.fmt.command",
            "productAgent.helper.command",
        ]
    );
}

/// The dormant declarations reach the same upgrade path as the active ones, so the
/// refusal is a trust question and not a ban on writing the key at all.
#[test]
fn a_trusted_checkout_may_declare_a_dormant_host_command() {
    let fixture = Fixture::new(
        r#"{
    "model": "provider/model",
    "lsp": {"custom": {"command": ["./bin/lsp"], "extensions": [".zx"], "disabled": true}}
}"#,
    );
    let grant = trust_roots(&[&fixture.root]);
    let fixture = fixture.with_trusted_global(&grant);

    let config = discover_with(&fixture.options())
        .expect("an explicitly trusted checkout may declare a disabled command too");
    assert_eq!(config.model.as_deref(), Some("provider/model"));
}

#[test]
fn a_project_layer_that_names_no_command_needs_no_trust() {
    // Given: a checkout that only selects a model and a remote MCP server.
    let fixture = Fixture::new(
        r#"{"model":"provider/model","mcp":{"docs":{"type":"remote","url":"https://example.invalid/mcp"}}}"#,
    );

    // When/Then: nothing runs on this machine, so nothing is refused.
    let config = discover_with(&fixture.options()).expect("a command-free project layer is fine");
    assert_eq!(config.model.as_deref(), Some("provider/model"));
}

/// Trust roots compare after canonicalisation, so a symlinked checkout is the same
/// checkout.
#[test]
#[cfg(unix)]
fn a_trusted_root_reached_through_a_symlink_is_the_same_root() {
    let fixture = Fixture::new(PROJECT_HOST_COMMANDS);
    let link = fixture.root.join("link-to-project");
    std::os::unix::fs::symlink(&fixture.project, &link).expect("symlink the checkout");
    let grant = trust_roots(&[&link]);
    let fixture = fixture.with_trusted_global(&grant);

    let config = discover_with(&fixture.options())
        .expect("a trust root that resolves to this checkout admits it");
    assert_eq!(config.shell.as_deref(), Some("./bin/project-shell"));
}
