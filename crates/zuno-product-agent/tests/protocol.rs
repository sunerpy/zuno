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
            // A product that cannot speak the protocol explains itself on stderr and nowhere else.
            // The exit that classifies the handshake therefore has to drain the reader it started,
            // both so the user is told what the installed product said and so the reader is not left
            // running after `run` returned.
            let rendered = error.to_string();
            assert!(
                rendered.contains("not a recognised subcommand"),
                "the product's own stderr must reach the incompatibility diagnostic: {rendered}"
            );
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
    // The fixture's own failure text carries no denial vocabulary, so this can only have been
    // decided by `turn.error.codexErrorInfo`.
    let rendered = error.to_string();
    assert!(
        !rendered.contains("denied: the sandbox rejected"),
        "unexpected fixture text: {rendered}"
    );
    assert!(rendered.contains("sandboxError"), "{rendered}");
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

#[tokio::test]
async fn cancellation_settles_when_an_escaped_grandchild_still_holds_stderr() {
    // A product that detaches a helper into its own process group, an MCP server for instance,
    // keeps Zuno's stderr pipe open after the guarded tree has been reaped. Draining that pipe
    // until EOF would strand `run`, so the cancellation the user asked for would never settle and
    // the product-agent tool call would never finish.
    for (kind, permission) in [
        (ProductAgentKind::Codex, ProductAgentPermissionMode::Never),
        (
            ProductAgentKind::ClaudeCode,
            ProductAgentPermissionMode::DontAsk,
        ),
    ] {
        let (error, pid) = cancel_hanging_phase(kind, permission, "hang-escaped").await;
        assert!(
            matches!(error, ProductAgentError::Cancelled { .. }),
            "{error}"
        );
        assert!(
            process_exists(pid),
            "the escaped grandchild was expected to outlive the group kill"
        );
        let _ignored = zuno_process::request_contained_process_shutdown(pid);
    }
}

#[tokio::test]
async fn a_failure_is_not_a_denial_because_stderr_mentions_permissions() {
    // The captured stderr belongs in the human-readable diagnostic, never in the verdict: an
    // unrelated warning about a file permission must not send the user to check approval settings
    // for a failure that approving nothing would fix.
    activate_guard();
    for (kind, permission) in [
        (ProductAgentKind::Codex, ProductAgentPermissionMode::Never),
        (
            ProductAgentKind::ClaudeCode,
            ProductAgentPermissionMode::DontAsk,
        ),
    ] {
        let root = tempfile::tempdir().expect("temporary root");
        let agent = configured(
            "stderr-denied",
            &config(
                kind,
                permission,
                "stderr-denied",
                &root.path().join("capture.json"),
                None,
            ),
            &environment(),
        )
        .expect("configured agent");
        let error = agent
            .run(request(root.path()), CancellationToken::new())
            .await
            .expect_err("fixture must fail");
        assert!(
            matches!(error, ProductAgentError::Failed { .. }),
            "{kind:?}: {error}"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("permission denied"),
            "the stderr must still be reported: {rendered}"
        );
    }
}

#[tokio::test]
async fn child_authored_failure_text_cannot_produce_a_permission_denial() {
    // Each case is an ordinary failure whose text a substring sniff reads as a refusal. The verdict
    // must come from each protocol's own typed field instead, because the failure text describes
    // whatever the turn was doing: repository paths, prompt text and model prose all reach it. If
    // text chose the label, a delegated task could talk the parent into believing the host's native
    // permissions refused something and had to be widened.
    activate_guard();
    for (kind, permission, mode, expected) in [
        (
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "denied-message-text",
            "failed to write /repo/permissions/mod.rs",
        ),
        (
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "denied-nested-field",
            "denied-cache",
        ),
        (
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "denied-object-code",
            "httpConnectionFailed",
        ),
        (
            ProductAgentKind::ClaudeCode,
            ProductAgentPermissionMode::DontAsk,
            "denied-message-text",
            "src/permissions/mod.rs",
        ),
    ] {
        let root = tempfile::tempdir().expect("temporary root");
        let agent = configured(
            "text-denial",
            &config(
                kind,
                permission,
                mode,
                &root.path().join("capture.json"),
                None,
            ),
            &environment(),
        )
        .expect("configured agent");
        let error = agent
            .run(request(root.path()), CancellationToken::new())
            .await
            .expect_err("fixture must fail");
        assert!(
            matches!(error, ProductAgentError::Failed { .. }),
            "{kind:?}/{mode}: {error}"
        );
        // The text is still reported, it just does not decide anything.
        let rendered = error.to_string();
        assert!(
            rendered.contains(expected),
            "{kind:?}/{mode}: `{expected}` missing from {rendered}"
        );
    }
}

#[tokio::test]
async fn a_prompt_larger_than_the_stdin_pipe_settles_when_the_product_stops_reading() {
    // The prompt is model-generated, so its size is not Zuno's to choose. A product that answers the
    // handshake and then stops draining stdin fills the pipe buffer, 64 KiB on Linux, and an
    // unbounded `write_all` never returns: `run` would never settle, no `tokio::select!` on the
    // cancellation token is ever reached again, and the product-agent tool call would hang for the
    // life of the session. The fixture holds its stdin read end open for two minutes, far longer
    // than this test waits, so only the caller's own ceiling can end the write.
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let agent = configured(
        "stdin-wedged",
        &config(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "stdin-wedged",
            &root.path().join("capture.json"),
            None,
        ),
        &environment(),
    )
    .expect("configured Codex");
    let error = tokio::time::timeout(
        Duration::from_secs(20),
        agent.run(
            ProductAgentRequest {
                prompt: "x".repeat(256 * 1024),
                description: None,
                directory: root.path().to_owned(),
            },
            CancellationToken::new(),
        ),
    )
    .await
    .expect("the invocation must settle instead of waiting on the wedged product")
    .expect_err("wedged stdin");
    // Uncertain, because a partly flushed line may already have carried a complete request.
    assert!(error.is_uncertain(), "{error}");
    let rendered = error.to_string();
    assert!(rendered.contains("did not accept"), "{rendered}");
}

#[tokio::test]
async fn a_refused_tool_call_the_product_worked_around_is_still_a_result() {
    // The fixture replays a real Claude Code 2.1.258 result frame, captured by running the product
    // with the flags this adapter passes and asking it to write `/etc` with Bash: the tool call was
    // refused and booked in `permission_denials`, and the turn still ended
    // `{"subtype":"success","is_error":false}` with the refusal quoted inside `result`.
    // `permission_denials` records every refusal in the turn, not the turn's outcome, so reading the
    // record without the outcome would turn that real, successful run into a permission failure the
    // caller has to recover from.
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let agent = configured(
        "denied-then-answered",
        &config(
            ProductAgentKind::ClaudeCode,
            ProductAgentPermissionMode::DontAsk,
            "denied-then-answered",
            &root.path().join("capture.json"),
            None,
        ),
        &environment(),
    )
    .expect("configured Claude");
    let result = agent
        .run(request(root.path()), CancellationToken::new())
        .await
        .expect("Claude result");
    assert!(
        result
            .text
            .contains("Permission to use Bash has been denied"),
        "the product's own answer must be reported verbatim: {}",
        result.text
    );
}

#[tokio::test]
async fn a_real_codex_sandbox_refusal_is_reported_as_the_products_own_answer() {
    // The fixture replays a real Codex 0.150.1 `turn/completed`, captured by driving the installed
    // app-server with the parameters this adapter sends (`approvalPolicy: never`,
    // `sandbox: workspace-write`) and asking it to write `/etc/zuno-denial-probe`. The refusal
    // reached neither `turn.error` (null) nor stderr (empty): it existed only as the model's final
    // answer on a turn whose status was `completed`. That is what a native refusal really looks
    // like, so it must be handed back as the product's answer. Classifying it from text would make a
    // successful turn a failure, and would let any answer that quotes a permission error decide that
    // the host's permissions had to be widened.
    activate_guard();
    let root = tempfile::tempdir().expect("temporary root");
    let agent = configured(
        "sandbox-refusal-in-answer",
        &config(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "sandbox-refusal-in-answer",
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
    assert_eq!(
        result.text,
        "zsh:1: read-only file system: /etc/zuno-denial-probe"
    );
}

#[tokio::test]
async fn a_turn_the_product_did_not_report_as_failed_is_never_a_permission_denial() {
    // `permission_denials` and `codexErrorInfo` record what happened to individual tool calls during
    // a turn; neither states the turn's outcome. The verdict must therefore be selected by the same
    // value that describes it: the product's own statement that the turn failed. Otherwise a turn
    // the product reports as successful is labelled a permission failure because its answer happened
    // to be blank, and the parent model is handed `permission denied` with no diagnostic content to
    // contradict — a delegated or prompt-injected turn only has to get one tool call refused and then
    // answer with whitespace.
    activate_guard();
    // Every case is reported, not just the first, so one run says which of the three shapes regressed.
    let mut wrong = Vec::new();
    for (kind, permission, mode, expected) in [
        // The real Claude Code 2.1.258 success frame with a whitespace answer: `is_error` false,
        // `subtype` `success`, one refused Bash call on the record.
        (
            ProductAgentKind::ClaudeCode,
            ProductAgentPermissionMode::DontAsk,
            "denied-then-blank",
            "result subtype `success`",
        ),
        // The Codex sibling of the same shape: `status` `completed`, a whitespace `agentMessage`, and
        // a populated typed refusal code the schema says a completed turn never carries.
        (
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "completed-blank-denied",
            "sandboxError",
        ),
        // And the unresolvable case: no status at all. Whether the turn failed is what the denial
        // depends on, so an absent status fails closed rather than defaulting to the verdict.
        (
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            "unstated-status-denied",
            "connection reset by peer",
        ),
    ] {
        let root = tempfile::tempdir().expect("temporary root");
        let agent = configured(
            "blank-answer",
            &config(
                kind,
                permission,
                mode,
                &root.path().join("capture.json"),
                None,
            ),
            &environment(),
        )
        .expect("configured agent");
        let error = agent
            .run(request(root.path()), CancellationToken::new())
            .await
            .expect_err("a blank answer is not a result");
        let rendered = error.to_string();
        if !matches!(error, ProductAgentError::Failed { .. }) {
            wrong.push(format!("{kind:?}/{mode}: wrong verdict: {rendered}"));
        }
        // The diagnostic must still say something. A blank answer reported verbatim is a verdict
        // with nothing in it at all for the caller to check.
        if !rendered.contains(expected) {
            wrong.push(format!(
                "{kind:?}/{mode}: `{expected}` missing from {rendered}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn a_handshake_response_missing_its_id_still_reaps_and_drains() {
    // The two exits the reviewer's cancel input does not reach, found by walking every exit that
    // holds the stderr reader: a response that omits the id it is defined to carry. Both used to
    // return with `?`, which reaped nothing — the guarded group was left to `kill_on_drop`, which
    // reaches only the direct child — and dropped the reader, which then looped for the life of the
    // helper the product had detached. The classification each reports is unchanged.
    activate_guard();
    for (mode, uncertain) in [("thread-start-no-id", false), ("turn-start-no-id", true)] {
        let baseline = alive_tasks();
        let root = tempfile::tempdir().expect("temporary root");
        let pid_path = root.path().join("child.pid");
        let agent = configured(
            "missing-id",
            &config(
                ProductAgentKind::Codex,
                ProductAgentPermissionMode::Never,
                mode,
                &root.path().join("capture.json"),
                Some(&pid_path),
            ),
            &environment(),
        )
        .expect("configured Codex");
        let error = tokio::time::timeout(
            Duration::from_secs(20),
            agent.run(request(root.path()), CancellationToken::new()),
        )
        .await
        .expect("a response the caller cannot use must settle")
        .expect_err("a response without its id is not a result");
        assert_eq!(error.is_uncertain(), uncertain, "{mode}: {error}");
        wait_for_path(&pid_path).await;
        let pid = std::fs::read_to_string(&pid_path)
            .expect("child pid")
            .parse::<u32>()
            .expect("numeric pid");
        assert!(
            process_exists(pid),
            "{mode}: the escaped helper must outlive the reap, otherwise the pipe would reach EOF \
             on its own and a leaked reader would exit without being observed"
        );
        tokio::time::sleep(Duration::from_millis(750)).await;
        assert_eq!(
            alive_tasks(),
            baseline,
            "{mode}: a stderr reader was still running after `run` returned"
        );
        let _ignored = zuno_process::request_contained_process_shutdown(pid);
    }
}

/// Tasks alive on this test's runtime, which is where `run` spawns its stderr reader.
///
/// The reader returns only at EOF, so one whose handle was dropped instead of aborted is still
/// alive, still holding the pipe read end and its 64 KiB buffer. The runtime already counts it, so
/// no production hook is needed to see it.
fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

#[tokio::test]
async fn cancelling_a_pre_stream_phase_leaves_no_detached_stderr_reader() {
    // Both exits that classify a failure raised before the stream loop, reached by the input that
    // makes the leak observable: cancel while the product is wedged in the handshake, or with the
    // turn response outstanding, after it detached a helper that inherited Zuno's stderr pipe. The
    // guarded group kill never reaches that helper, so the pipe stays open and a reader that was
    // dropped rather than aborted keeps running for the rest of the helper's life.
    for (mode, uncertain) in [
        ("hang-initialize-escaped", false),
        ("hang-turn-start-escaped", true),
    ] {
        let baseline = alive_tasks();
        let (error, pid) = cancel_hanging_phase(
            ProductAgentKind::Codex,
            ProductAgentPermissionMode::Never,
            mode,
        )
        .await;
        // The ceilings must not have changed what the phase is reported as.
        assert_eq!(error.is_uncertain(), uncertain, "{mode}: {error}");
        assert!(
            process_exists(pid),
            "{mode}: the escaped helper must outlive the group kill, otherwise the pipe would reach \
             EOF on its own and a leaked reader would exit without being observed"
        );
        tokio::time::sleep(Duration::from_millis(750)).await;
        assert_eq!(
            alive_tasks(),
            baseline,
            "{mode}: a stderr reader was still running after `run` returned"
        );
        let _ignored = zuno_process::request_contained_process_shutdown(pid);
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
