#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use zuno_config::schema::product_agent::{
    ProductAgentConfig, ProductAgentKind, ProductAgentPermissionMode,
};
use zuno_paths::Env;
use zuno_product_agent::{ProductAgentError, ProductAgentRequest, configured};

static ACTIVATE_GUARD: Once = Once::new();
const SECRET: &str = "fixture-secret-value";

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zuno-product-agent-fixture"))
}

fn activate_guard() {
    ACTIVATE_GUARD.call_once(|| {
        zuno_process::activate_guard_executable(fixture()).expect("activate fixture guard");
    });
}

fn config(
    kind: ProductAgentKind,
    permission_mode: ProductAgentPermissionMode,
    mode: &str,
    capture: &Path,
    child_pid: Option<&Path>,
) -> ProductAgentConfig {
    let mut env = BTreeMap::from([
        (
            "ZUNO_PRODUCT_AGENT_FIXTURE_MODE".to_owned(),
            mode.to_owned(),
        ),
        (
            "ZUNO_PRODUCT_AGENT_CAPTURE".to_owned(),
            capture.display().to_string(),
        ),
    ]);
    if let Some(child_pid) = child_pid {
        env.insert(
            "ZUNO_PRODUCT_AGENT_CHILD_PID".to_owned(),
            child_pid.display().to_string(),
        );
    }
    ProductAgentConfig {
        kind,
        enabled: Some(true),
        command: Some(fixture().display().to_string()),
        tool_name: None,
        permission_mode: Some(permission_mode),
        env: Some(env),
    }
}

fn environment() -> Env {
    Env::empty()
        .with("HTTP_PROXY", "http://proxy.fixture:8080")
        .with("NO_PROXY", "localhost,127.0.0.1")
        .with("SECRET_TOKEN", SECRET)
}

fn request(directory: &Path) -> ProductAgentRequest {
    ProductAgentRequest {
        prompt: "perform the fixture task".to_owned(),
        description: Some("fixture".to_owned()),
        directory: directory.to_owned(),
    }
}

fn capture(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("capture file")).expect("capture JSON")
}

#[tokio::test]
async fn codex_uses_app_server_and_inherits_cwd_proxy_and_native_environment() {
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let capture_path = root.path().join("capture.json");
    let agent = configured(
        "codex",
        &config(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "normal",
            &capture_path,
            None,
        ),
        &environment(),
    )
    .expect("configured Codex");

    let result = agent
        .run(request(root.path()), CancellationToken::new())
        .await
        .expect("Codex result");
    assert_eq!(result.text, "codex final answer");
    let captured = capture(&capture_path);
    assert_eq!(
        captured["args"],
        serde_json::json!(["app-server", "--stdio"])
    );
    assert_eq!(captured["cwd"], root.path().display().to_string());
    assert_eq!(captured["httpProxy"], "http://proxy.fixture:8080");
    assert_eq!(captured["noProxy"], "localhost,127.0.0.1");
    assert_eq!(captured["secret"], SECRET);
    assert_eq!(
        captured.pointer("/threadStartRequests/0/params"),
        Some(&serde_json::json!({
            "cwd": root.path(),
            "ephemeral": true,
            "approvalPolicy": "never",
            "sandbox": "workspaceWrite"
        }))
    );
    assert_eq!(
        captured.pointer("/turnStartRequests/0/params"),
        Some(&serde_json::json!({
            "threadId": "thr_fixture",
            "input": [{"type":"text","text":"perform the fixture task"}]
        }))
    );
}

#[tokio::test]
async fn codex_declines_unattended_approval_and_reports_only_the_final_answer() {
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let agent = configured(
        "codex",
        &config(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::OnRequest,
            "approval",
            &root.path().join("capture.json"),
            None,
        ),
        &environment(),
    )
    .expect("configured Codex");
    let result = agent
        .run(request(root.path()), CancellationToken::new())
        .await
        .expect("Codex result");
    assert_eq!(result.text, "approval declined safely");
}

#[tokio::test]
async fn codex_retries_the_legacy_enum_dialect_only_after_invalid_params() {
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let capture_path = root.path().join("capture.json");
    let agent = configured(
        "codex",
        &config(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::OnRequest,
            "legacy",
            &capture_path,
            None,
        ),
        &environment(),
    )
    .expect("configured Codex");

    let result = agent
        .run(request(root.path()), CancellationToken::new())
        .await
        .expect("legacy Codex result");
    assert_eq!(result.text, "codex final answer");
    let captured = capture(&capture_path);
    let starts = captured["threadStartRequests"]
        .as_array()
        .expect("thread starts");
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[0].pointer("/params/approvalPolicy"),
        Some(&serde_json::json!("onRequest"))
    );
    assert_eq!(
        starts[0].pointer("/params/sandbox"),
        Some(&serde_json::json!("workspaceWrite"))
    );
    assert_eq!(
        starts[1].pointer("/params/approvalPolicy"),
        Some(&serde_json::json!("on-request"))
    );
    assert_eq!(
        starts[1].pointer("/params/sandbox"),
        Some(&serde_json::json!("workspace-write"))
    );
}

#[tokio::test]
async fn codex_classifies_incompatible_and_lost_protocols_without_replay() {
    activate_guard();
    for (mode, incompatible) in [("incompatible", true), ("eof", false)] {
        let root = tempfile::tempdir().expect("temporary root");
        let agent = configured(
            "codex",
            &config(
                ProductAgentKind::Codex,
                ProductAgentPermissionMode::Never,
                mode,
                &root.path().join("capture.json"),
                None,
            ),
            &environment(),
        )
        .expect("configured Codex");
        let error = agent
            .run(request(root.path()), CancellationToken::new())
            .await
            .expect_err("fixture must fail");
        if incompatible {
            assert!(matches!(error, ProductAgentError::Incompatible { .. }));
        } else {
            assert!(error.is_uncertain(), "{error}");
        }
    }
}

#[tokio::test]
async fn codex_permission_denial_is_typed() {
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let agent = configured(
        "codex",
        &config(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "permission-denied",
            &root.path().join("capture.json"),
            None,
        ),
        &environment(),
    )
    .expect("configured Codex");

    let error = agent
        .run(request(root.path()), CancellationToken::new())
        .await
        .expect_err("permission denial");
    assert!(matches!(error, ProductAgentError::Denied { .. }), "{error}");
}

#[tokio::test]
async fn claude_uses_one_shot_stream_json_and_native_permission_mode() {
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let capture_path = root.path().join("capture.json");
    let agent = configured(
        "claude",
        &config(
            ProductAgentKind::ClaudeCode,
            ProductAgentPermissionMode::DontAsk,
            "normal",
            &capture_path,
            None,
        ),
        &environment(),
    )
    .expect("configured Claude");
    let result = agent
        .run(request(root.path()), CancellationToken::new())
        .await
        .expect("Claude result");
    assert_eq!(result.text, "claude final answer");
    let captured = capture(&capture_path);
    let args = captured["args"]
        .as_array()
        .expect("captured args")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    for expected in [
        "--print",
        "--verbose",
        "stream-json",
        "--no-session-persistence",
        "dontAsk",
        "AskUserQuestion",
    ] {
        assert!(args.contains(&expected), "missing `{expected}` in {args:?}");
    }
}

#[tokio::test]
async fn claude_permission_denial_is_typed_and_malformed_output_redacts_secrets() {
    activate_guard();
    for mode in ["permission-denied", "malformed"] {
        let root = tempfile::tempdir().expect("temporary root");
        let agent = configured(
            "claude",
            &config(
                ProductAgentKind::ClaudeCode,
                ProductAgentPermissionMode::DontAsk,
                mode,
                &root.path().join("capture.json"),
                None,
            ),
            &environment(),
        )
        .expect("configured Claude");
        let error = agent
            .run(request(root.path()), CancellationToken::new())
            .await
            .expect_err("fixture must fail");
        if mode == "permission-denied" {
            assert!(matches!(error, ProductAgentError::Denied { .. }), "{error}");
        } else {
            assert!(error.is_uncertain(), "{error}");
            let rendered = error.to_string();
            assert!(!rendered.contains(SECRET), "{rendered}");
            assert!(rendered.contains("[REDACTED"), "{rendered}");
        }
    }
}

#[tokio::test]
async fn cancellation_reaps_the_guarded_process_tree_for_both_products() {
    // Mid-turn: the Codex turn id is known, so the streaming loop interrupts that exact turn, and
    // Claude Code's one-shot stream is simply torn down. Both are observed cancellations.
    for (kind, permission) in [
        (ProductAgentKind::Codex, ProductAgentPermissionMode::Never),
        (
            ProductAgentKind::ClaudeCode,
            ProductAgentPermissionMode::DontAsk,
        ),
    ] {
        let (error, pid) = cancel_hanging_phase(kind, permission, "hang").await;
        assert!(
            matches!(error, ProductAgentError::Cancelled { .. }),
            "{error}"
        );
        assert_process_reaped(pid).await;
    }
}

/// Run one fixture mode to the point where it hangs, then cancel it.
///
/// Returns the failure and the pid of the fixture's own grandchild, so a caller can assert both
/// the classification and that the guarded tree was reaped.
async fn cancel_hanging_phase(
    kind: ProductAgentKind,
    permission: ProductAgentPermissionMode,
    mode: &str,
) -> (ProductAgentError, u32) {
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let pid_path = root.path().join("child.pid");
    let agent = configured(
        "cancel",
        &config(
            kind,
            permission,
            mode,
            &root.path().join("capture.json"),
            Some(&pid_path),
        ),
        &environment(),
    )
    .expect("configured agent");
    let cancellation = CancellationToken::new();
    let run = tokio::spawn({
        let cancellation = cancellation.clone();
        let directory = root.path().to_owned();
        async move { agent.run(request(&directory), cancellation).await }
    });
    wait_for_path(&pid_path).await;
    let pid = std::fs::read_to_string(&pid_path)
        .expect("child pid")
        .parse::<u32>()
        .expect("numeric pid");
    assert!(process_exists(pid), "fixture child never became live");

    cancellation.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("cancellation completed")
        .expect("runner task")
        .expect_err("cancelled invocation");
    (error, pid)
}

async fn assert_process_reaped(pid: u32) {
    for _ in 0..100 {
        if !process_exists(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("guarded child process {pid} survived");
}

#[tokio::test]
async fn cancelling_before_the_turn_is_requested_is_reported_as_cancellation() {
    // `initialize` and `thread/start` ask the product for nothing that touches the working
    // directory, so a cancellation there is a user interruption that pauses, never a protocol
    // incompatibility that would blame the user's installation and block the goal.
    for mode in ["hang-initialize", "hang-thread-start"] {
        let (error, pid) = cancel_hanging_phase(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            mode,
        )
        .await;
        assert!(
            matches!(error, ProductAgentError::Cancelled { .. }),
            "{mode}: {error}"
        );
        assert!(!error.is_uncertain(), "{mode}: {error}");
        assert_process_reaped(pid).await;
    }
}

#[tokio::test]
async fn cancelling_an_outstanding_turn_start_response_stays_uncertain() {
    // The other side of the same line: `turn/start` is already flushed, so the product may have
    // begun editing files and Zuno has no turn id to interrupt. The outcome is unknown and must
    // not be replayed mechanically, so it is uncertain rather than a clean cancellation.
    let (error, pid) = cancel_hanging_phase(
        ProductAgentKind::Codex,
        ProductAgentPermissionMode::Never,
        "hang-turn-start",
    )
    .await;
    assert!(error.is_uncertain(), "{error}");
    assert!(
        !matches!(error, ProductAgentError::Cancelled { .. }),
        "{error}"
    );
    let rendered = error.to_string();
    assert!(rendered.contains("cancelled"), "{rendered}");
    assert!(rendered.contains("turn/start"), "{rendered}");
    assert_process_reaped(pid).await;
}

#[tokio::test]
async fn a_prompt_that_looks_like_an_option_is_passed_after_the_terminator() {
    activate_guard();
    for prompt in ["-not-an-option", "--dangerously-skip-permissions"] {
        let root = tempfile::tempdir().expect("temporary root");
        let capture_path = root.path().join("capture.json");
        let agent = configured(
            "claude",
            &config(
                ProductAgentKind::ClaudeCode,
                ProductAgentPermissionMode::DontAsk,
                "normal",
                &capture_path,
                None,
            ),
            &environment(),
        )
        .expect("configured Claude");
        let result = agent
            .run(
                ProductAgentRequest {
                    prompt: prompt.to_owned(),
                    description: None,
                    directory: root.path().to_owned(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("Claude result");
        assert_eq!(result.text, "claude final answer");

        let captured = capture(&capture_path);
        let args = captured["args"]
            .as_array()
            .expect("captured args")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let terminator = args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or_else(|| panic!("no `--` terminator in {args:?}"));
        assert_eq!(
            args[terminator + 1..],
            [prompt.to_owned()],
            "the prompt must be the only operand: {args:?}"
        );
        // Nothing Zuno did not choose may be read as an option, so the prompt text must not
        // appear anywhere the child would still parse flags.
        assert!(
            !args[..terminator].iter().any(|argument| argument == prompt),
            "prompt text reached the option side of the argv: {args:?}"
        );
    }
}

async fn wait_for_path(path: &Path) {
    for _ in 0..300 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fixture did not create {}", path.display());
}

fn process_exists(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}
