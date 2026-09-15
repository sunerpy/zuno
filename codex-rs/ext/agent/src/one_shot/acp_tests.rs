#![allow(clippy::expect_used)]

use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn fixture_script(directory: &TempDir, body: &str) -> PathBuf {
    let path = directory.path().join("fake-acp.sh");
    std::fs::write(&path, body).expect("write ACP fixture");
    let mut permissions = std::fs::metadata(&path)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("make ACP fixture executable");
    path
}

fn request(directory: &TempDir, prompt: &str) -> OneShotAgentRequest {
    OneShotAgentRequest {
        prompt: prompt.to_owned(),
        cwd: AbsolutePathBuf::from_absolute_path(directory.path()).expect("absolute tempdir"),
    }
}

#[tokio::test]
async fn acp_backend_runs_handshake_profile_options_and_strict_final_text() {
    let directory = TempDir::new().expect("tempdir");
    let log = directory.path().join("requests.jsonl");
    let script = fixture_script(
        &directory,
        r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$ACP_TEST_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"acp-session-1"}}'
      ;;
    *'"method":"session/set_model"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{}}'
      ;;
    *'"configId":"reasoning_effort"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{}}'
      ;;
    *'"configId":"mode"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":5,"result":{}}'
      ;;
    *'"configId":"permissions"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":6,"result":{}}'
      ;;
    *'"method":"session/prompt"'*)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acp-session-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"strict final"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":7,"result":{"stopReason":"end_turn"}}'
      ;;
  esac
done
"#,
    );
    let backend = AcpBackend::new(AcpBackendConfig {
        executable: script,
        model: Some("configured-model".to_owned()),
        model_provider: Some("configured-provider".to_owned()),
        reasoning_effort: Some("high".to_owned()),
        mode: Some("plan".to_owned()),
        permissions: Some("read-only".to_owned()),
        env: BTreeMap::from([(
            "ACP_TEST_LOG".to_owned(),
            log.to_string_lossy().into_owned(),
        )]),
        run_timeout: Some(Duration::from_secs(5)),
        ..AcpBackendConfig::default()
    })
    .expect("valid ACP backend");

    let result = backend
        .run(request(&directory, "review this"), CancellationToken::new())
        .await
        .expect("ACP run succeeds");
    assert_eq!(result.backend, OneShotAgentBackendKind::Acp);
    assert_eq!(result.final_answer, "strict final");
    assert_eq!(result.product_session_id.as_deref(), Some("acp-session-1"));

    let requests = std::fs::read_to_string(log).expect("request log");
    for expected in [
        "\"method\":\"initialize\"",
        "\"method\":\"session/new\"",
        "\"model\":\"configured-model\"",
        "\"modelProvider\":\"configured-provider\"",
        "\"method\":\"session/set_model\"",
        "\"configId\":\"reasoning_effort\"",
        "\"configId\":\"mode\"",
        "\"configId\":\"permissions\"",
        "\"method\":\"session/prompt\"",
    ] {
        assert!(
            requests.contains(expected),
            "missing {expected} in {requests}"
        );
    }
}

#[tokio::test]
async fn acp_backend_cancellation_notifies_and_terminates_owned_process() {
    let directory = TempDir::new().expect("tempdir");
    let script = fixture_script(
        &directory,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
      ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"cancel-session"}}'
      ;;
    *'"method":"session/prompt"'*)
      : > "$ACP_PROMPT_MARKER"
      sleep 30
      ;;
  esac
done
"#,
    );
    let marker = directory.path().join("prompt-started");
    let backend = AcpBackend::new(AcpBackendConfig {
        executable: script,
        env: BTreeMap::from([(
            "ACP_PROMPT_MARKER".to_owned(),
            marker.to_string_lossy().into_owned(),
        )]),
        run_timeout: Some(Duration::from_secs(20)),
        dispose_grace: Duration::from_millis(100),
        ..AcpBackendConfig::default()
    })
    .expect("valid ACP backend");
    let cancellation = CancellationToken::new();
    let run_cancellation = cancellation.clone();
    let run_request = request(&directory, "wait");
    let run = tokio::spawn(async move { backend.run(run_request, run_cancellation).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !marker.is_file() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt was dispatched before cancellation");
    cancellation.cancel();

    let error = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("cancelled process settles")
        .expect("ACP run task joins")
        .expect_err("cancelled ACP run fails");
    assert_eq!(error.backend, OneShotAgentBackendKind::Acp);
    assert_eq!(error.stage, OneShotAgentFailureStage::Run);
    assert_eq!(error.category, OneShotAgentFailureCategory::Aborted);
}

#[tokio::test]
async fn acp_backend_rejects_executable_drift_before_spawn() {
    let directory = TempDir::new().expect("tempdir");
    let marker = directory.path().join("spawned");
    let script = fixture_script(
        &directory,
        &format!("#!/bin/sh\n: > '{}'\n", marker.display()),
    );
    let backend = AcpBackend::new(AcpBackendConfig {
        executable: script,
        executable_sha256: Some("0".repeat(64)),
        ..AcpBackendConfig::default()
    })
    .expect("well-formed digest config");

    let error = backend
        .run(
            request(&directory, "must not start"),
            CancellationToken::new(),
        )
        .await
        .expect_err("changed executable must fail closed");

    assert_eq!(error.stage, OneShotAgentFailureStage::Validate);
    assert_eq!(error.category, OneShotAgentFailureCategory::InvalidRequest);
    assert!(!marker.exists(), "drifted executable must not be spawned");
}

#[test]
fn acp_backend_rejects_unbounded_or_ambiguous_profile_values() {
    for config in [
        AcpBackendConfig::default(),
        AcpBackendConfig {
            executable: PathBuf::from("agent"),
            mode: Some(" plan ".to_owned()),
            ..AcpBackendConfig::default()
        },
        AcpBackendConfig {
            executable: PathBuf::from("agent"),
            max_message_bytes: 0,
            ..AcpBackendConfig::default()
        },
    ] {
        let error = AcpBackend::new(config).expect_err("invalid config rejected");
        assert_eq!(error.stage, OneShotAgentFailureStage::Validate);
        assert_eq!(error.category, OneShotAgentFailureCategory::InvalidRequest);
    }
}
