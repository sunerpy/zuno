//! Live rust-analyzer coverage for [`Manager::diagnostics`].
//!
//! The point of a live test is that the server is real: a stub cannot regress the
//! way a released rust-analyzer can. That makes the environment part of the
//! subject, so this file models **three** states rather than two:
//!
//! 1. **absent** — no `rust-analyzer` on `PATH`. An ordinary skip.
//! 2. **present but cannot be brought up** — a name resolves, yet the process is
//!    not a working language server. A skip too, but one that must say the server
//!    id and the underlying error out loud, because it looks identical to a pass.
//! 3. **up** — the server initialized and published diagnostics. The assertion
//!    runs, and a wrong answer is a failure.
//!
//! Collapsing 2 into 1 is how this test used to fail on CI: `which` finds
//! `~/.cargo/bin/rust-analyzer`, but that is a rustup *proxy* that exists whether
//! or not the `rust-analyzer` component was ever installed. Without the component
//! it exits `1` immediately with `Unknown binary 'rust-analyzer' in official
//! toolchain`, the supervisor burns its restart budget in a few seconds, and
//! [`ManagerError::Unavailable`] surfaced as a red test that said nothing about
//! the missing component.
//!
//! Collapsing 2 into 3 would be worse: the test would go green on a machine that
//! never ran a language server.

use serde_json::{Value, json};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use zuno_catalog::lsp_config::ResolvedLsp;
use zuno_config::schema::lsp::{BUILTIN_SERVER_IDS, LspConfig};
use zuno_lsp::{Diagnostic, Manager, ManagerError, RestartPolicy, ServerRegistry};

/// Why a live server cannot be exercised here, phrased to complete
/// `SKIPPED {test}: {reason}`.
enum Unusable {
    /// Nothing by that name is on `PATH`.
    Missing,
    /// A path resolved, but the program behind it is not a usable server.
    Broken(String),
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str("not found on PATH"),
            Self::Broken(reason) => formatter.write_str(reason),
        }
    }
}

/// Resolve `name` on `PATH` and prove the program can at least run.
///
/// The `--version` probe is what separates state 1 from state 2 for the common
/// case of a rustup proxy standing in for an uninstalled component: the proxy
/// resolves, then refuses. Naming the exit status and stderr keeps the eventual
/// skip diagnosable instead of merely quiet.
fn usable_server(name: &str) -> Result<PathBuf, Unusable> {
    let path = which::which(name).map_err(|_| Unusable::Missing)?;
    match Command::new(&path).arg("--version").output() {
        Err(error) => Err(Unusable::Broken(format!(
            "{} could not be executed: {error}",
            path.display()
        ))),
        Ok(output) if !output.status.success() => Err(Unusable::Broken(format!(
            "{} exited with {} for `--version`: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
        Ok(_) => Ok(path),
    }
}

/// Ask Zuno's manager for diagnostics, keeping [`ManagerError`] intact.
///
/// The error type is deliberately not boxed: the caller has to tell
/// [`ManagerError::Unavailable`] (the server never came up — state 2) apart from
/// every other failure (a real defect).
async fn rust_diagnostics(
    workspace: &Path,
    source: &Path,
    config: &LspConfig,
) -> Result<Vec<Diagnostic>, ManagerError> {
    let registry = Arc::new(ServerRegistry::offline(&ResolvedLsp::resolve(Some(config))));
    let manager = Manager::new(workspace, registry, RestartPolicy::default());
    let diagnostics = manager.diagnostics(source).await;
    manager.shutdown().await;
    diagnostics
}

/// Every built-in disabled except `server_id`, which is pinned to `command`, so
/// the test measures one known process instead of whatever the host provisions.
fn isolated_server_config(server_id: &str, command: &[String]) -> Value {
    let mut servers = serde_json::Map::new();
    for id in BUILTIN_SERVER_IDS {
        servers.insert((*id).to_owned(), json!({ "disabled": true }));
    }
    servers.insert(server_id.to_owned(), json!({ "command": command }));
    Value::Object(servers)
}

/// A real rust-analyzer over a cold, dependency-free crate must report the
/// deliberate `E0425`.
///
/// The [`ManagerError::Unavailable`] arm is the state-2 net for a server that
/// passes the `--version` probe yet never completes `initialize` — a stub that
/// answers `--version` and then stays silent lands here, not in a red run. The
/// arm cannot hide a broken handshake in Zuno itself, because the handshake is
/// covered hermetically by the stub-server tests in `zuno-lsp`'s own unit tests.
#[tokio::test]
async fn rust_analyzer_reports_a_live_compiler_error() -> Result<(), Box<dyn Error>> {
    let server = match usable_server("rust-analyzer") {
        Ok(path) => path,
        Err(reason) => {
            eprintln!(
                "SKIPPED rust_analyzer_reports_a_live_compiler_error: rust-analyzer {reason}; \
                 live Rust diagnostics were NOT exercised"
            );
            return Ok(());
        }
    };
    let workspace = tempfile::tempdir()?;
    let source_dir = workspace.path().join("src");
    std::fs::create_dir(&source_dir)?;
    let source = source_dir.join("main.rs");
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"zuno_lsp_live\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(&source, "fn main() { does_not_exist(); }\n")?;
    let command = vec![server.to_string_lossy().into_owned()];
    let config: LspConfig = serde_json::from_value(isolated_server_config("rust", &command))?;

    let actual = match rust_diagnostics(workspace.path(), &source, &config).await {
        Ok(diagnostics) => diagnostics,
        Err(error @ ManagerError::Unavailable { .. }) => {
            eprintln!(
                "SKIPPED rust_analyzer_reports_a_live_compiler_error: {error} — {} never \
                 completed initialization; live Rust diagnostics were NOT exercised",
                server.display()
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };

    assert!(
        actual.iter().any(|diagnostic| {
            diagnostic.severity == Some(1)
                && (diagnostic.code == Some(json!("E0425"))
                    || diagnostic.message.contains("does_not_exist"))
        }),
        "rust-analyzer diagnostics did not include the deliberate E0425 error: {actual:?}"
    );
    Ok(())
}
