use std::path::{Path, PathBuf};
use std::process::Command;

const SDK_ENTRY: &str =
    "opencode/packages/opencode/node_modules/@agentclientprotocol/sdk/dist/acp.js";

fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(manifest_dir)
        .to_path_buf()
}

fn sdk_entry_from(repository_root: &Path) -> Option<PathBuf> {
    let candidate = repository_root.join(SDK_ENTRY);
    candidate.is_file().then_some(candidate)
}

#[test]
fn real_sdk_drives_streaming_permission_and_cancellation_with_pure_stdout() {
    let repository_root = workspace_root();
    let Some(sdk_entry) = sdk_entry_from(&repository_root) else {
        println!(
            "SKIP: pinned @agentclientprotocol/sdk 0.21.0 is absent at {}; the live SDK \
             test requires a repository-local fixture and will not search parent directories",
            repository_root.join(SDK_ENTRY).display()
        );
        return;
    };
    let output = Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live_sdk_client.mjs"))
        .arg(env!("CARGO_BIN_EXE_oc-acp-conformance"))
        .arg(sdk_entry)
        .output()
        .expect("node runs the real ACP SDK client");
    assert!(
        output.status.success(),
        "real SDK test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("SDK harness emits one JSON report");
    assert_eq!(report["sdkVersion"], "0.21.0");
    assert_eq!(report["protocolVersion"], 1);
    assert_eq!(report["permissionRequests"], 1);
    assert_eq!(report["normalStopReason"], "end_turn");
    assert_eq!(report["cancelStopReason"], "cancelled");
    assert_eq!(report["cancelFinalUpdateBeforeResponse"], true);
    assert!(report["frames"].as_u64().is_some_and(|count| count >= 12));
    assert_eq!(report["stdoutWasPureNdjson"], true);
    assert_eq!(report["stderrWasNonEmpty"], true);
}

#[test]
fn sdk_discovery_does_not_escape_the_repository_root() {
    let temp = tempfile::tempdir().expect("temporary discovery tree");
    let repository_root = temp.path().join("zuno");
    std::fs::create_dir_all(&repository_root).expect("create synthetic repository");

    let external_sdk = temp.path().join(SDK_ENTRY);
    std::fs::create_dir_all(external_sdk.parent().expect("SDK entry has a parent"))
        .expect("create external SDK directory");
    std::fs::write(&external_sdk, "external fixture").expect("write external SDK fixture");

    assert_eq!(
        sdk_entry_from(&repository_root),
        None,
        "SDK discovery escaped the repository and accepted {}",
        external_sdk.display()
    );
}

#[test]
fn sdk_discovery_accepts_a_repository_local_fixture() {
    let temp = tempfile::tempdir().expect("temporary discovery tree");
    let repository_root = temp.path().join("zuno");
    let local_sdk = repository_root.join(SDK_ENTRY);
    std::fs::create_dir_all(local_sdk.parent().expect("SDK entry has a parent"))
        .expect("create repository-local SDK directory");
    std::fs::write(&local_sdk, "repository fixture").expect("write local SDK fixture");

    assert_eq!(sdk_entry_from(&repository_root), Some(local_sdk));
}
