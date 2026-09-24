use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::ffi::OsStr;
use std::path::Path;

#[test]
fn claude_result_requires_strict_non_error_success() {
    assert_eq!(
        decode_claude_result(
            br#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"session-1"}"#,
        )
        .expect("strict success"),
        OneShotAgentResult {
            backend: OneShotAgentBackendKind::ClaudeCode,
            final_answer: "done".to_string(),
            product_session_id: Some("session-1".to_string()),
        }
    );

    for invalid in [
        br#"{"type":"assistant","subtype":"success","is_error":false,"result":"done"}"#.as_slice(),
        br#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"failed"}"#.as_slice(),
        br#"{"type":"result","subtype":"success","is_error":false,"result":"  "}"#.as_slice(),
        br#"not-json"#.as_slice(),
    ] {
        assert_eq!(
            decode_claude_result(invalid).expect_err("invalid result"),
            OneShotAgentError::new(
                OneShotAgentBackendKind::ClaudeCode,
                OneShotAgentFailureStage::Decode,
                OneShotAgentFailureCategory::InvalidResult,
            )
        );
    }
}

#[test]
fn claude_command_is_fixed_by_provider_configuration() {
    let backend = ClaudeCodeBackend::new(ClaudeCodeBackendConfig {
        model: Some("opus".to_string()),
        reasoning_effort: Some("high".to_string()),
        permission_mode: ClaudeCodePermissionMode::Plan,
        ..ClaudeCodeBackendConfig::default()
    })
    .expect("valid backend");
    assert_eq!(
        backend.command_args(),
        [
            "--print",
            "--output-format",
            "json",
            "--no-session-persistence",
            "--permission-mode",
            "plan",
            "--disallowedTools",
            "AskUserQuestion,ExitPlanMode",
            "--model",
            "opus",
            "--effort",
            "high",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
    );
}

#[test]
fn claude_backend_rejects_a_zero_run_timeout() {
    let error = ClaudeCodeBackend::new(ClaudeCodeBackendConfig {
        run_timeout: Some(Duration::ZERO),
        ..ClaudeCodeBackendConfig::default()
    })
    .expect_err("zero timeout must fail before process start");
    assert_eq!(
        error,
        OneShotAgentError::new(
            OneShotAgentBackendKind::ClaudeCode,
            OneShotAgentFailureStage::Validate,
            OneShotAgentFailureCategory::InvalidRequest,
        )
    );
}

#[test]
fn credential_env_names_are_fail_closed() {
    for name in [
        "OPENAI_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "PRIVATE_TOKEN",
        "DB_PASSWORD",
        "CLIENT_SECRET",
        "SERVICE_CREDENTIALS",
    ] {
        assert!(super::super::is_credential_env_name(name), "{name}");
    }
    for name in ["PATH", "HOME", "HTTP_PROXY", "RUST_LOG"] {
        assert!(!super::super::is_credential_env_name(name), "{name}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn restricted_external_process_is_wrapped_by_the_linux_sandbox() {
    let directory = tempfile::tempdir().expect("temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(directory.path()).expect("absolute temp dir");
    let helper = directory.path().join("codex-linux-sandbox");
    let executable = directory.path().join("external-agent");
    let sandbox = OneShotProcessSandboxConfig {
        permission_profile: codex_protocol::models::PermissionProfile::read_only(),
        workspace_roots: vec![cwd.clone()],
        codex_home: cwd.clone(),
        codex_self_exe: None,
        codex_linux_sandbox_exe: Some(helper.clone()),
        managed_network_configured: false,
        use_legacy_landlock: false,
        windows_sandbox_type: codex_sandboxing::SandboxType::None,
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
    };

    let command = prepare_external_process_command(
        OneShotAgentBackendKind::Acp,
        &executable,
        vec![OsString::from("--stdio")],
        &BTreeMap::new(),
        &cwd,
        Some(&sandbox),
    )
    .expect("restricted process is wrapped");
    let command = command.as_std();
    assert_eq!(command.get_program(), helper.as_os_str());
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|arg| arg == "--permission-profile"));
    assert!(args.iter().any(|arg| arg == &executable.to_string_lossy()));
    assert_eq!(args.last().map(String::as_str), Some("--stdio"));
}

#[cfg(target_os = "linux")]
#[test]
fn restricted_external_process_fails_closed_without_the_linux_sandbox_helper() {
    let directory = tempfile::tempdir().expect("temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(directory.path()).expect("absolute temp dir");
    let sandbox = OneShotProcessSandboxConfig {
        permission_profile: codex_protocol::models::PermissionProfile::read_only(),
        workspace_roots: vec![cwd.clone()],
        codex_home: cwd.clone(),
        codex_self_exe: None,
        codex_linux_sandbox_exe: None,
        managed_network_configured: false,
        use_legacy_landlock: false,
        windows_sandbox_type: codex_sandboxing::SandboxType::None,
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
    };

    let error = prepare_external_process_command(
        OneShotAgentBackendKind::ClaudeCode,
        Path::new("claude"),
        Vec::new(),
        &BTreeMap::new(),
        &cwd,
        Some(&sandbox),
    )
    .expect_err("restricted process must not fall back to native execution");
    assert_eq!(error.stage, OneShotAgentFailureStage::Validate);
    assert_eq!(error.category, OneShotAgentFailureCategory::AccessPolicy);
}

#[test]
fn explicit_full_access_external_process_stays_native() {
    let directory = tempfile::tempdir().expect("temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(directory.path()).expect("absolute temp dir");
    let sandbox = OneShotProcessSandboxConfig {
        permission_profile: codex_protocol::models::PermissionProfile::Disabled,
        workspace_roots: vec![cwd.clone()],
        codex_home: cwd.clone(),
        codex_self_exe: None,
        codex_linux_sandbox_exe: None,
        managed_network_configured: false,
        use_legacy_landlock: false,
        windows_sandbox_type: codex_sandboxing::SandboxType::None,
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
    };

    let command = prepare_external_process_command(
        OneShotAgentBackendKind::ClaudeCode,
        Path::new("claude"),
        vec![OsString::from("--print")],
        &BTreeMap::new(),
        &cwd,
        Some(&sandbox),
    )
    .expect("full access is an explicit native launch");
    let command = command.as_std();
    assert_eq!(command.get_program(), "claude");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("--print")]
    );
}

#[test]
fn external_process_does_not_bypass_a_managed_network_proxy() {
    let directory = tempfile::tempdir().expect("temp dir");
    let cwd = AbsolutePathBuf::from_absolute_path(directory.path()).expect("absolute temp dir");
    let sandbox = OneShotProcessSandboxConfig {
        permission_profile: codex_protocol::models::PermissionProfile::Disabled,
        workspace_roots: vec![cwd.clone()],
        codex_home: cwd.clone(),
        codex_self_exe: None,
        codex_linux_sandbox_exe: None,
        managed_network_configured: true,
        use_legacy_landlock: false,
        windows_sandbox_type: codex_sandboxing::SandboxType::None,
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
    };

    let error = prepare_external_process_command(
        OneShotAgentBackendKind::Acp,
        Path::new("agent-acp"),
        Vec::new(),
        &BTreeMap::new(),
        &cwd,
        Some(&sandbox),
    )
    .expect_err("managed network policy must not be silently bypassed");
    assert_eq!(error.stage, OneShotAgentFailureStage::Validate);
    assert_eq!(error.category, OneShotAgentFailureCategory::AccessPolicy);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires ZUNO_SANDBOX_E2E_HELPER pointing at a packaged zuno executable"]
async fn workspace_profile_confines_the_external_process_on_linux() {
    use codex_protocol::permissions::NetworkSandboxPolicy;

    let helper = std::env::var_os("ZUNO_SANDBOX_E2E_HELPER")
        .map(PathBuf::from)
        .expect("ZUNO_SANDBOX_E2E_HELPER must be set");
    let workspace = tempfile::tempdir().expect("workspace temp dir");
    let outside = tempfile::tempdir().expect("outside temp dir");
    let denied_path = outside.path().join("must-not-exist");
    let cwd = AbsolutePathBuf::from_absolute_path(workspace.path()).expect("absolute workspace");
    let sandbox = OneShotProcessSandboxConfig {
        permission_profile: codex_protocol::models::PermissionProfile::workspace_write_with(
            &[],
            NetworkSandboxPolicy::Restricted,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        ),
        workspace_roots: vec![cwd.clone()],
        codex_home: cwd.clone(),
        codex_self_exe: None,
        codex_linux_sandbox_exe: Some(helper),
        managed_network_configured: false,
        use_legacy_landlock: false,
        windows_sandbox_type: codex_sandboxing::SandboxType::None,
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
    };
    let script = format!(
        "touch '{}' 2>/dev/null || true; if test -e '{}'; then printf WRITTEN; else printf DENIED; fi",
        denied_path.display(),
        denied_path.display()
    );
    let mut command = prepare_external_process_command(
        OneShotAgentBackendKind::Acp,
        Path::new("/bin/sh"),
        vec![OsString::from("-c"), OsString::from(script)],
        &BTreeMap::new(),
        &cwd,
        Some(&sandbox),
    )
    .expect("prepare sandboxed process");
    let output = command.output().await.expect("run sandboxed process");

    assert!(output.status.success(), "sandbox helper failed: {output:?}");
    assert_eq!(output.stdout, b"DENIED");
    assert!(!denied_path.exists(), "external write escaped workspace");
}

#[cfg(unix)]
#[tokio::test]
async fn claude_backend_executes_one_strict_result() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temp dir");
    let executable = directory.path().join("claude-fixture");
    std::fs::write(
        &executable,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"fixture answer\",\"session_id\":\"fixture-session\"}'\n",
    )
    .expect("write fixture");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).expect("make fixture executable");

    let backend = ClaudeCodeBackend::new(ClaudeCodeBackendConfig {
        executable,
        ..ClaudeCodeBackendConfig::default()
    })
    .expect("valid backend");
    let result = backend
        .run(
            OneShotAgentRequest {
                prompt: "inspect the workspace".to_string(),
                cwd: AbsolutePathBuf::from_absolute_path(directory.path())
                    .expect("absolute fixture path"),
            },
            CancellationToken::new(),
        )
        .await
        .expect("fixture succeeds");

    assert_eq!(
        result,
        OneShotAgentResult {
            backend: OneShotAgentBackendKind::ClaudeCode,
            final_answer: "fixture answer".to_string(),
            product_session_id: Some("fixture-session".to_string()),
        }
    );
}

#[cfg(unix)]
#[tokio::test]
async fn claude_backend_cancellation_settles_as_aborted() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temp dir");
    let executable = directory.path().join("claude-fixture");
    std::fs::write(&executable, "#!/bin/sh\ncat >/dev/null\nsleep 30\n").expect("write fixture");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).expect("make fixture executable");

    let backend = ClaudeCodeBackend::new(ClaudeCodeBackendConfig {
        executable,
        dispose_grace: Duration::from_millis(100),
        ..ClaudeCodeBackendConfig::default()
    })
    .expect("valid backend");
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    let error = timeout(
        Duration::from_secs(2),
        backend.run(
            OneShotAgentRequest {
                prompt: "wait".to_string(),
                cwd: AbsolutePathBuf::from_absolute_path(directory.path())
                    .expect("absolute fixture path"),
            },
            cancellation,
        ),
    )
    .await
    .expect("cancellation is bounded")
    .expect_err("run must abort");

    assert_eq!(
        error,
        OneShotAgentError::new(
            OneShotAgentBackendKind::ClaudeCode,
            OneShotAgentFailureStage::Run,
            OneShotAgentFailureCategory::Aborted,
        )
    );
}

#[cfg(unix)]
#[tokio::test]
async fn claude_backend_run_timeout_terminates_the_process_tree() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temp dir");
    let executable = directory.path().join("claude-fixture");
    std::fs::write(&executable, "#!/bin/sh\ncat >/dev/null\nsleep 30\n").expect("write fixture");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).expect("make fixture executable");

    let backend = ClaudeCodeBackend::new(ClaudeCodeBackendConfig {
        executable,
        run_timeout: Some(Duration::from_millis(20)),
        dispose_grace: Duration::from_millis(100),
        ..ClaudeCodeBackendConfig::default()
    })
    .expect("valid backend");
    let error = timeout(
        Duration::from_secs(2),
        backend.run(
            OneShotAgentRequest {
                prompt: "wait".to_string(),
                cwd: AbsolutePathBuf::from_absolute_path(directory.path())
                    .expect("absolute fixture path"),
            },
            CancellationToken::new(),
        ),
    )
    .await
    .expect("run timeout is bounded")
    .expect_err("run must time out");

    assert_eq!(
        error,
        OneShotAgentError::new(
            OneShotAgentBackendKind::ClaudeCode,
            OneShotAgentFailureStage::Run,
            OneShotAgentFailureCategory::Limit,
        )
    );
}
