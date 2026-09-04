use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tempfile::tempdir;
use tokio::sync::Notify;
use zuno_extension::{
    API_VERSION, ExtensionRegistry, Package, Scope, StaticPackage, resolve_active, runtime_surface,
};
use zuno_runtime::{HarnessProfile, HarnessRuntime};
use zuno_tool::{AllowAll, InterruptHandle, NeverInterrupted, ToolContext};

/// A user interrupt the test fires only once the plugin confirms it owns the call.
///
/// Racing a sleep against the plugin would decide which cancellation branch is exercised
/// by scheduling luck; the fixture writes a marker file when it receives `tools/call`, so
/// the test can fire the interrupt at a known point in the protocol.
struct Cancel {
    fired: AtomicBool,
    notify: Notify,
}

impl Cancel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fired: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    fn fire(&self) {
        self.fired.store(true, Ordering::Release);
        self.notify.notify_waiters();
        self.notify.notify_one();
    }
}

#[async_trait]
impl InterruptHandle for Cancel {
    fn is_set(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    async fn notified(&self) {
        while !self.fired.load(Ordering::Acquire) {
            self.notify.notified().await;
        }
    }
}

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
    context_with(Arc::new(NeverInterrupted))
}

fn context_with(interrupt: Arc<dyn InterruptHandle>) -> ToolContext {
    ToolContext::new(
        "ses_plugin",
        "msg_plugin",
        "call_plugin",
        "build",
        Arc::new(AllowAll),
        interrupt,
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

#[tokio::test]
async fn cancelling_a_dispatched_process_call_reports_an_undecided_outcome() {
    let Some(python) = python() else {
        eprintln!("python is unavailable; process plugin fixture skipped");
        return;
    };
    let fixture = tempdir().expect("fixture");
    let package = write_process_package_with_script(
        fixture.path(),
        &python,
        "cancelled-process",
        r#"
import json
import sys
import time

for raw in sys.stdin:
    request = json.loads(raw)
    method = request["method"]
    if method == "tools/call":
        with open("dispatched", "w", encoding="utf-8") as marker:
            marker.write("1")
        time.sleep(600)
    result = {"protocolVersion": "zuno.plugin/1"} if method == "initialize" else {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
    if method == "shutdown":
        break
"#,
    );
    let dispatched = fixture.path().join("cancelled-process").join("dispatched");
    let extensions = resolve_active(
        &Scope::new(fixture.path()),
        &[package],
        &ExtensionRegistry::new(),
    )
    .expect("resolved package");
    let mut surface = runtime_surface(&extensions, fixture.path()).expect("runtime surface");
    let tool = Arc::clone(&surface.tools()[0]);
    let profile = HarnessProfile::new("cancelled-plugin-test")
        .with_bundle(surface.take_bundle().expect("runtime bundle"));
    let runtime = HarnessRuntime::new("cancelled-plugin-test");
    runtime
        .activate_profile(profile)
        .await
        .expect("process host initializes");

    let interrupt = Cancel::new();
    let call = tokio::spawn({
        let interrupt = Arc::clone(&interrupt);
        async move {
            tool.invoke(
                json!({"subject": "cancelled work"}),
                context_with(interrupt),
            )
            .await
        }
    });
    for _ in 0..500 {
        if dispatched.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        dispatched.exists(),
        "the fixture never reported receiving the call"
    );
    interrupt.fire();

    let output = call
        .await
        .expect("the tool task settles")
        .expect("a cancelled call settles as a report, not a failure");
    assert_eq!(
        output.title, "review_outline cancelled",
        "the client card names the cancelled tool, not a JSON-RPC method"
    );
    let cancellation = &output.metadata["cancellation"];
    assert_eq!(cancellation["cancelled"], json!(true));
    assert_eq!(cancellation["uncertain"], json!(true), "{cancellation}");
    assert_eq!(cancellation["authoritative"], json!(false));
    assert_eq!(cancellation["dispatched"], json!(true), "{cancellation}");
    assert!(
        output.output.contains("Inspect the authoritative state"),
        "{}",
        output.output
    );
    runtime.shutdown().await.expect("retired host is quiescent");
}

/// A workspace no plugin boundary can carry is refused before any runtime starts.
///
/// Both hosts hand the workspace to the plugin as UTF-8 text, so an odd-byte path used to
/// panic inside `json!` on the process host and reach the guest lossily substituted on the
/// WASI host.
#[cfg(unix)]
#[test]
fn a_workspace_no_plugin_boundary_can_carry_is_refused() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let fixture = tempdir().expect("fixture");
    let package = write_process_package(fixture.path(), Path::new("python3"));
    let extensions = resolve_active(
        &Scope::new(fixture.path()),
        &[package],
        &ExtensionRegistry::new(),
    )
    .expect("resolved package");
    let workspace = PathBuf::from(OsStr::from_bytes(b"/tmp/zuno-\xff-workspace"));

    let Err(error) = runtime_surface(&extensions, &workspace) else {
        panic!("an unrepresentable workspace cannot reach a plugin");
    };

    assert!(error.to_string().contains("not valid UTF-8"), "{error}");
}

/// A backslash in a native path is a name, not a separator, on Linux and macOS.
///
/// `initialize`'s `packageRoot`/`workspace` is the only statement a process plugin gets
/// about which tree it owns, and that plugin runs unconfined with full native filesystem
/// access. A workspace literally named `zuno\ws` that reaches it as `zuno/ws` points it at
/// a different directory, so the boundary asserts the exact spelling rather than a
/// separator-normalised rendering of it.
#[cfg(unix)]
#[tokio::test]
async fn a_backslash_in_a_native_path_reaches_the_plugin_unchanged() {
    let Some(python) = python() else {
        eprintln!("python is unavailable; process plugin fixture skipped");
        return;
    };
    let fixture = tempdir().expect("fixture");
    let workspace = fixture.path().join(r"zuno\ws");
    fs::create_dir_all(&workspace).expect("a directory whose name contains a backslash");
    let package = write_process_package_with_script(
        &workspace,
        &python,
        "path-echo-process",
        r#"
import json
import sys

seen = {}
for raw in sys.stdin:
    request = json.loads(raw)
    method = request["method"]
    if method == "initialize":
        seen["workspace"] = request["params"]["workspace"]
        seen["packageRoot"] = request["params"]["packageRoot"]
        result = {"protocolVersion": "zuno.plugin/1"}
    elif method == "tools/call":
        result = {"title": "paths", "output": json.dumps(seen), "metadata": {}}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
    if method == "shutdown":
        break
"#,
    );
    let package_root = workspace.join("path-echo-process");
    let extensions = resolve_active(
        &Scope::new(&workspace),
        &[package],
        &ExtensionRegistry::new(),
    )
    .expect("resolved package");
    let mut surface = runtime_surface(&extensions, &workspace).expect("runtime surface");
    let tool = Arc::clone(&surface.tools()[0]);
    let profile = HarnessProfile::new("path-echo-plugin-test")
        .with_bundle(surface.take_bundle().expect("runtime bundle"));
    let runtime = HarnessRuntime::new("path-echo-plugin-test");
    runtime
        .activate_profile(profile)
        .await
        .expect("process host initializes");

    let output = tool
        .invoke(json!({"subject": "paths"}), context())
        .await
        .expect("tool call succeeds");
    let seen: serde_json::Value =
        serde_json::from_str(&output.output).expect("the fixture echoed initialize parameters");

    assert_eq!(
        seen["workspace"].as_str(),
        workspace.to_str(),
        "the plugin was told it owns a different tree: {seen}"
    );
    assert_eq!(
        seen["packageRoot"].as_str(),
        package_root.to_str(),
        "the plugin was told its package lives elsewhere: {seen}"
    );
    runtime.shutdown().await.expect("process host stops");
}

/// A cancellation that beat the request out of the host reports that nothing ran.
///
/// The certain branch is a positive claim to the model — "the call was stopped before the
/// plugin received it" — so it needs a host-level test of its own, not only the metadata
/// unit test. The fixture stops reading stdin after `initialize`, so a request larger than
/// the pipe buffer can never be flushed and the outcome is decided by the protocol rather
/// than by which arm of the select happened to be polled first.
#[tokio::test]
async fn cancelling_a_call_the_plugin_never_received_reports_that_nothing_ran() {
    let Some(python) = python() else {
        eprintln!("python is unavailable; process plugin fixture skipped");
        return;
    };
    let fixture = tempdir().expect("fixture");
    let package = write_process_package_with_script(
        fixture.path(),
        &python,
        "undelivered-process",
        r#"
import json
import sys
import time

for raw in sys.stdin:
    request = json.loads(raw)
    method = request["method"]
    result = {"protocolVersion": "zuno.plugin/1"} if method == "initialize" else {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
    if method == "initialize":
        time.sleep(600)
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
    let profile = HarnessProfile::new("undelivered-plugin-test")
        .with_bundle(surface.take_bundle().expect("runtime bundle"));
    let runtime = HarnessRuntime::new("undelivered-plugin-test");
    runtime
        .activate_profile(profile)
        .await
        .expect("process host initializes");

    // `activate_profile` returned, so the fixture answered `initialize` and stopped
    // reading. A request that cannot fit in the pipe buffer therefore never completes its
    // flush, and the interrupt is already pending before the first poll.
    let interrupt = Cancel::new();
    interrupt.fire();
    let output = tool
        .invoke(
            json!({"subject": "x".repeat(512 * 1024)}),
            context_with(interrupt),
        )
        .await
        .expect("a cancelled call settles as a report, not a failure");

    assert_eq!(output.title, "review_outline cancelled");
    let cancellation = &output.metadata["cancellation"];
    assert_eq!(cancellation["uncertain"], json!(false), "{cancellation}");
    assert_eq!(cancellation["authoritative"], json!(true), "{cancellation}");
    assert_eq!(cancellation["dispatched"], json!(false), "{cancellation}");
    assert_eq!(cancellation.get("stopped"), None, "{cancellation}");
    assert!(output.output.contains("nothing ran"), "{}", output.output);
    assert!(
        output.output.contains("review_outline"),
        "{}",
        output.output
    );
    runtime.shutdown().await.expect("retired host is quiescent");
}
