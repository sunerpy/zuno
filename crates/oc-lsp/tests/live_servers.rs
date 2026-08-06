use oc_catalog::lsp_config::ResolvedLsp;
use oc_config::schema::lsp::{BUILTIN_SERVER_IDS, LspConfig};
use oc_lsp::{Diagnostic, Manager, RestartPolicy, ServerRegistry};
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct ComparableDiagnostic {
    message: String,
    severity: Option<u32>,
    code: Option<Value>,
    source: Option<String>,
    range: ComparableRange,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct ComparableRange {
    start: ComparablePosition,
    end: ComparablePosition,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct ComparablePosition {
    line: u32,
    character: u32,
}

impl From<&Diagnostic> for ComparableDiagnostic {
    fn from(value: &Diagnostic) -> Self {
        Self {
            message: value.message.clone(),
            severity: value.severity,
            code: value.code.clone(),
            source: value.source.clone(),
            range: ComparableRange {
                start: ComparablePosition {
                    line: value.range.start.line,
                    character: value.range.start.character,
                },
                end: ComparablePosition {
                    line: value.range.end.line,
                    character: value.range.end.character,
                },
            },
        }
    }
}

fn command_path(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

fn oracle_path() -> Option<PathBuf> {
    let pinned = PathBuf::from("/config/.local/share/mise/installs/opencode/1.18.12/opencode");
    pinned
        .is_file()
        .then_some(pinned)
        .or_else(|| command_path("opencode"))
}

fn oracle_diagnostics(
    oracle: &Path,
    workspace: &Path,
    source: &Path,
    server_id: &str,
    command: &[String],
) -> Result<Vec<ComparableDiagnostic>, Box<dyn Error>> {
    let lsp = isolated_server_config(server_id, command);
    let config = json!({
        "lsp": lsp
    });
    let output = Command::new(oracle)
        .args(["debug", "lsp", "diagnostics"])
        .arg(source)
        .env("OPENCODE_CONFIG_CONTENT", serde_json::to_string(&config)?)
        .current_dir(workspace)
        .output()?;
    if !output.status.success() {
        return Err(format!("oracle failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    let by_path: serde_json::Map<String, Value> = serde_json::from_slice(&output.stdout)?;
    let value = by_path
        .get(&source.to_string_lossy().into_owned())
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    Ok(serde_json::from_value(value)?)
}

async fn rust_diagnostics(
    workspace: &Path,
    source: &Path,
    server_id: &str,
    command: &[String],
) -> Result<Vec<ComparableDiagnostic>, Box<dyn Error>> {
    let config: LspConfig = serde_json::from_value(isolated_server_config(server_id, command))?;
    let registry = Arc::new(ServerRegistry::offline(&ResolvedLsp::resolve(Some(
        &config,
    ))));
    let manager = Manager::new(workspace, registry, RestartPolicy::default());
    let diagnostics = manager.diagnostics(source).await?;
    manager.shutdown().await;
    Ok(diagnostics.iter().map(ComparableDiagnostic::from).collect())
}

fn isolated_server_config(server_id: &str, command: &[String]) -> Value {
    let mut servers = serde_json::Map::new();
    for id in BUILTIN_SERVER_IDS {
        servers.insert((*id).to_owned(), json!({ "disabled": true }));
    }
    servers.insert(server_id.to_owned(), json!({ "command": command }));
    Value::Object(servers)
}

#[tokio::test]
async fn typescript_diagnostics_match_the_real_opencode_binary() -> Result<(), Box<dyn Error>> {
    let Some(server) = command_path("typescript-language-server") else {
        eprintln!(
            "skipping live TypeScript LSP differential: typescript-language-server not found"
        );
        return Ok(());
    };
    let Some(oracle) = oracle_path() else {
        eprintln!("skipping live TypeScript LSP differential: opencode oracle binary not found");
        return Ok(());
    };
    let workspace = tempfile::tempdir()?;
    let source = workspace.path().join("main.ts");
    std::fs::write(
        workspace.path().join("package.json"),
        r#"{"name":"oc-lsp-live","private":true}"#,
    )?;
    std::fs::write(&source, "const value: number = \"wrong\";\n")?;
    let command = vec![server.to_string_lossy().into_owned(), "--stdio".to_owned()];

    let expected = oracle_diagnostics(&oracle, workspace.path(), &source, "typescript", &command)?;
    let actual = rust_diagnostics(workspace.path(), &source, "typescript", &command).await?;

    assert!(
        !actual.is_empty(),
        "the live TypeScript server returned no diagnostic"
    );
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn rust_analyzer_reports_a_live_compiler_error() -> Result<(), Box<dyn Error>> {
    let Some(server) = command_path("rust-analyzer") else {
        eprintln!("skipping live Rust LSP test: rust-analyzer not found");
        return Ok(());
    };
    let workspace = tempfile::tempdir()?;
    let source_dir = workspace.path().join("src");
    std::fs::create_dir(&source_dir)?;
    let source = source_dir.join("main.rs");
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"oc_lsp_live\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(&source, "fn main() { does_not_exist(); }\n")?;
    let command = vec![server.to_string_lossy().into_owned()];

    let actual = rust_diagnostics(workspace.path(), &source, "rust", &command).await?;

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
