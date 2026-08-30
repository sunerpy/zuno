use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tempfile::tempdir;
use zuno_extension::{
    API_VERSION, ExtensionRegistry, Package, Scope, StaticPackage, resolve_active, runtime_surface,
};
use zuno_runtime::{HarnessProfile, HarnessRuntime};
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};

fn python() -> Option<PathBuf> {
    ["python3", "python"].into_iter().find_map(|candidate| {
        std::process::Command::new(candidate)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|_| PathBuf::from(candidate))
    })
}

fn write_process_package(root: &Path, command: &Path) -> StaticPackage {
    write_process_package_with_script(
        root,
        command,
        "process-fixture",
        include_str!("../../../examples/plugins/process-review/plugin.py"),
    )
}

fn write_process_package_with_script(
    root: &Path,
    command: &Path,
    id: &str,
    script: &str,
) -> StaticPackage {
    let package_root = root.join(id);
    fs::create_dir_all(&package_root).expect("package root");
    fs::write(package_root.join("plugin.py"), script).expect("fixture plugin");
    let package: Package = serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": id,
        "description": "process host fixture",
        "runtime": {
            "kind": "process",
            "command": command,
            "args": ["plugin.py"],
            "capabilities": ["host.full"],
            "timeoutMs": 5000
        },
        "tools": {
            "review_outline": {
                "description": "Create a review outline",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string"}
                    },
                    "required": ["subject"],
                    "additionalProperties": false
                },
                "effect": "sideEffecting",
                "replay": "never"
            }
        }
    }))
    .expect("valid process package");
    StaticPackage::new(package, package_root.join("extension.json"))
        .expect("matching package provenance")
}

fn context() -> ToolContext {
    ToolContext::new(
        "ses_plugin",
        "msg_plugin",
        "call_plugin",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[tokio::test]
async fn process_plugin_mounts_invokes_and_stops_with_the_profile() {
    let Some(python) = python() else {
        eprintln!("python is unavailable; process plugin fixture skipped");
        return;
    };
    let fixture = tempdir().expect("fixture");
    let package = write_process_package(fixture.path(), &python);
    let extensions = resolve_active(
        &Scope::new(fixture.path()),
        &[package],
        &ExtensionRegistry::new(),
    )
    .expect("resolved package");
    let mut surface = runtime_surface(&extensions, fixture.path()).expect("runtime surface");
    let tool = Arc::clone(&surface.tools()[0]);
    let profile = HarnessProfile::new("plugin-test")
        .with_bundle(surface.take_bundle().expect("runtime bundle"));
    let runtime = HarnessRuntime::new("plugin-test");
    runtime
        .activate_profile(profile)
        .await
        .expect("process host initializes");

    let output = tool
        .invoke(json!({"subject": "plugin lifecycle"}), context())
        .await
        .expect("tool call succeeds");

    assert_eq!(output.title, "Review outline");
    assert!(output.output.contains("plugin lifecycle"));
    assert_eq!(output.metadata["example"], true);
    runtime.shutdown().await.expect("process host stops");
    let error = tool
        .invoke(json!({"subject": "after shutdown"}), context())
        .await
        .expect_err("withdrawn host cannot be called");
    assert!(error.to_string().contains("failed"));
}

#[tokio::test]
async fn malformed_process_results_retire_the_untrusted_host() {
    use std::error::Error as _;

    let Some(python) = python() else {
        eprintln!("python is unavailable; process plugin fixture skipped");
        return;
    };
    let fixture = tempdir().expect("fixture");
    let package = write_process_package_with_script(
        fixture.path(),
        &python,
        "malformed-process",
        r#"
import json
import sys

for raw in sys.stdin:
    request = json.loads(raw)
    method = request["method"]
    if method == "initialize":
        result = {"protocolVersion": "zuno.plugin/1"}
    elif method == "tools/call":
        result = {"title": "missing required output", "metadata": {}}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
    if method == "shutdown":
        break
"#,
    );
    let extensions = resolve_active(
        &Scope::new(fixture.path()),
        &[package],
        &ExtensionRegistry::new(),
    )
    .expect("resolved package");
    let mut surface = runtime_surface(&extensions, fixture.path()).expect("runtime surface");
    let tool = Arc::clone(&surface.tools()[0]);
    let profile = HarnessProfile::new("malformed-plugin-test")
        .with_bundle(surface.take_bundle().expect("runtime bundle"));
    let runtime = HarnessRuntime::new("malformed-plugin-test");
    runtime
        .activate_profile(profile)
        .await
        .expect("process host initializes");

    let first = tool
        .invoke(json!({"subject": "malformed"}), context())
        .await
        .expect_err("malformed result is uncertain");
    let first_source = first
        .source()
        .expect("tool failure keeps its source")
        .to_string();
    assert!(first_source.contains("uncertain"), "{first_source}");

    let second = tool
        .invoke(json!({"subject": "must not run"}), context())
        .await
        .expect_err("retired host cannot accept another call");
    let second_source = second
        .source()
        .expect("tool failure keeps its source")
        .to_string();
    assert!(
        second_source.contains("no longer running"),
        "{second_source}"
    );
    runtime.shutdown().await.expect("retired host is quiescent");
}

#[tokio::test]
#[ignore = "built and run by scripts/check-plugin-examples.sh"]
async fn wasi_plugin_fixture_negotiates_invokes_and_stops() {
    let artifact = PathBuf::from(
        std::env::var_os("ZUNO_WASI_TEST_COMPONENT")
            .expect("ZUNO_WASI_TEST_COMPONENT points at the built example"),
    );
    let fixture = tempdir().expect("fixture");
    let package_root = fixture.path().join("wasi-fixture");
    fs::create_dir_all(&package_root).expect("package root");
    fs::copy(&artifact, package_root.join("plugin.wasm")).expect("copy component");
    let package: Package = serde_json::from_value(json!({
        "apiVersion": API_VERSION,
        "id": "wasi-fixture",
        "description": "WASI host fixture",
        "runtime": {
            "kind": "wasi",
            "artifact": "plugin.wasm",
            "capabilities": [],
            "environment": [],
            "fuel": 10_000_000,
            "memoryMiB": 64,
            "timeoutMs": 5_000
        },
        "tools": {
            "word_count": {
                "description": "Count words",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"],
                    "additionalProperties": false
                },
                "effect": "readOnly",
                "replay": "safe"
            }
        }
    }))
    .expect("valid WASI package");
    let package = StaticPackage::new(package, package_root.join("extension.json"))
        .expect("matching package provenance");
    let extensions = resolve_active(
        &Scope::new(fixture.path()),
        &[package],
        &ExtensionRegistry::new(),
    )
    .expect("resolved package");
    let mut surface = runtime_surface(&extensions, fixture.path()).expect("runtime surface");
    let tool = Arc::clone(&surface.tools()[0]);
    let profile = HarnessProfile::new("wasi-plugin-test")
        .with_bundle(surface.take_bundle().expect("runtime bundle"));
    let runtime = HarnessRuntime::new("wasi-plugin-test");
    runtime
        .activate_profile(profile)
        .await
        .expect("WASI component initializes");

    let output = tool
        .invoke(json!({"text": "one two three four"}), context())
        .await
        .expect("WASI tool call succeeds");

    assert_eq!(output.title, "Word count");
    assert_eq!(output.output, "4");
    assert_eq!(output.metadata["words"], 4);
    runtime.shutdown().await.expect("WASI component stops");
}
