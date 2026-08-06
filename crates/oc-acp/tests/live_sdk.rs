use std::path::{Path, PathBuf};
use std::process::Command;

fn sdk_entry() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|ancestor| {
            ancestor.join(
                "opencode/packages/opencode/node_modules/@agentclientprotocol/sdk/dist/acp.js",
            )
        })
        .find(|candidate| candidate.is_file())
        .expect("the pinned oracle tree contains @agentclientprotocol/sdk")
}

#[test]
fn real_sdk_drives_streaming_permission_and_cancellation_with_pure_stdout() {
    let output = Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live_sdk_client.mjs"))
        .arg(env!("CARGO_BIN_EXE_oc-acp-conformance"))
        .arg(sdk_entry())
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
