//! The three shipped examples are discovered from a directory and their hook runs.
//!
//! Drives the real [`discover_process_plugins`] and [`load_plugins_ordered`], so a
//! change that breaks reachability fails here rather than in a user's terminal. Each
//! example writes a differently-named variable in one `shell.env` dispatch, which is
//! what makes "all three ran" a single assertion instead of three hopeful ones.

use std::path::{Path, PathBuf};
use std::process::Command;

use zuno_plugin::{
    ConfigDirectory, HookInvocation, PluginScope, ShellEnvInput, ShellEnvOutput,
    discover_process_plugins, load_plugins_ordered,
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonicalize the workspace root")
}

fn install_executable(directory: &Path, name: &str, source: &Path) {
    std::fs::create_dir_all(directory).expect("create the discovery directory");
    let destination = directory.join(name);
    std::fs::copy(source, &destination).expect("copy the example into the discovery directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mut permissions = std::fs::metadata(&destination)
            .expect("stat the installed example")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&destination, permissions).expect("mark the example executable");
    }
}

/// `go` spells it `go version`, with no dashes, and rejects `--version`.
///
/// Worth naming because the wrong probe fails exactly like an absent toolchain, and
/// this test reports that as "NOT verified on this host" — a permanent false skip
/// that still prints `ok`.
fn toolchain_present(program: &str, version_argument: &str) -> bool {
    Command::new(program)
        .arg(version_argument)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
#[tokio::test]
async fn every_shipped_example_is_discovered_and_contributes_its_shell_env() {
    // Given
    let root = workspace_root();
    let temp = tempfile::tempdir().expect("tempdir");
    let discovery = temp.path().join(".zuno").join("plugin");

    if !toolchain_present("go", "version") || !toolchain_present("node", "--version") {
        eprintln!(
            "SKIPPED every_shipped_example_is_discovered_and_contributes_its_shell_env: go or \
             node is absent, so the Go and JavaScript examples were NOT verified on this host"
        );
        return;
    }

    let go_binary = temp.path().join("go-example");
    let build = Command::new("go")
        .args(["build", "-o"])
        .arg(&go_binary)
        .arg("./examples/go_plugin")
        .current_dir(&root)
        .env("GOCACHE", temp.path().join("gocache"))
        .output()
        .expect("run go build");
    assert!(
        build.status.success(),
        "the Go example must build: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    install_executable(&discovery, "go-example", &go_binary);
    install_executable(&discovery, "js-example", &root.join("examples/js_plugin"));
    install_executable(
        &discovery,
        "rust-example",
        Path::new(env!("CARGO_BIN_EXE_zuno-example-plugin")),
    );

    // When
    let discovered = discover_process_plugins(&[ConfigDirectory::new(
        &temp.path().join(".zuno"),
        PluginScope::Local,
    )])
    .expect("discover the installed examples");
    let names: Vec<&str> = discovered
        .iter()
        .map(|plugin| plugin.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["go-example", "js-example", "rust-example"],
        "all three examples must be discovered by dropping them in a directory"
    );

    let specs = discovered
        .into_iter()
        .map(|plugin| {
            zuno_plugin::PluginProcessSpec::new(plugin.name, plugin.program)
                .timeout(std::time::Duration::from_secs(20))
        })
        .collect();
    let load = load_plugins_ordered(specs).await;
    assert!(
        load.diagnostics().is_empty(),
        "diagnostics={:?}",
        load.diagnostics()
    );

    let input = ShellEnvInput {
        cwd: "/workspace",
        session_id: Some("ses_examples"),
        call_id: Some("call_examples"),
    };
    let mut output = ShellEnvOutput::default();
    load.hook_bus()
        .dispatch(HookInvocation::ShellEnv {
            input: &input,
            output: &mut output,
        })
        .await
        .expect("dispatch shell.env to all three examples");

    // Then
    assert_eq!(
        output.env.get("GO_PLUGIN").map(String::as_str),
        Some("enabled"),
        "env={:?}",
        output.env
    );
    assert_eq!(
        output.env.get("JS_PLUGIN").map(String::as_str),
        Some("enabled"),
        "env={:?}",
        output.env
    );
    assert_eq!(
        output.env.get("RUST_PLUGIN").map(String::as_str),
        Some("enabled"),
        "env={:?}",
        output.env
    );
    load.shutdown().await;
}
