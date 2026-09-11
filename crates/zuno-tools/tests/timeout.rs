#[cfg(unix)]
mod support;

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(unix)]
use zuno_pty::{
    BackgroundExecutionId, BackgroundExecutionPurpose, BackgroundExecutionService,
    BackgroundExecutionStatus,
};
#[cfg(unix)]
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};
#[cfg(unix)]
use zuno_tools::shell::ShellParams;
use zuno_tools::timeout::{MAX_FOREGROUND_TIMEOUT_MS, normalize_foreground_timeout};

#[cfg(unix)]
use zuno_tool::{ExitAuthority, ReceiptOutcome, TypedTool, VerificationReceipt};
#[cfg(unix)]
use zuno_tools::{BackgroundAction, BackgroundParams, BackgroundTool};

#[cfg(unix)]
fn context() -> ToolContext {
    ToolContext::new(
        "ses_timeout",
        "msg_timeout",
        "call_timeout",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

#[cfg(unix)]
fn params(command: impl Into<String>, timeout: Option<u64>) -> ShellParams {
    ShellParams {
        command: command.into(),
        timeout,
        workdir: None,
        background: false,
        background_purpose: BackgroundExecutionPurpose::Command,
        expected_git_head: None,
        exit_policy: None,
    }
}

#[test]
fn timeout_policy_defaults_to_120_seconds_and_caps_requests_at_600_seconds() {
    assert_eq!(normalize_foreground_timeout(None), 120_000);
    assert_eq!(normalize_foreground_timeout(Some(42)), 42);
    assert_eq!(
        normalize_foreground_timeout(Some(MAX_FOREGROUND_TIMEOUT_MS + 1)),
        MAX_FOREGROUND_TIMEOUT_MS
    );
}

#[cfg(unix)]
fn foreground_read(action: BackgroundAction, id: &BackgroundExecutionId) -> BackgroundParams {
    BackgroundParams {
        action,
        task_id: Some(id.as_str().to_owned()),
        cursor: Some(0),
        limit: None,
        timeout: matches!(action, BackgroundAction::Wait).then_some(10),
        output_path: None,
    }
}

#[cfg(unix)]
async fn launch_resumable_foreground(
    workspace: &Path,
    service: &Arc<BackgroundExecutionService>,
) -> zuno_tool::ToolOutput {
    let mut request = params(
        "printf launch >> launches && cat release >/dev/null && printf verified",
        Some(10),
    );
    request.exit_policy = Some(zuno_tools::shell::ExitPolicy::All);
    launch_resumable_request(workspace, service, request, Duration::from_secs(30)).await
}

#[cfg(unix)]
async fn launch_resumable_request(
    workspace: &Path,
    service: &Arc<BackgroundExecutionService>,
    request: ShellParams,
    hard_ceiling: Duration,
) -> zuno_tool::ToolOutput {
    assert!(
        std::process::Command::new("mkfifo")
            .arg(workspace.join("release"))
            .status()
            .expect("create fixture gate")
            .success()
    );
    let tool = support::sandbox::configured_shell_tool(workspace, Some("/bin/bash"))
        .with_background_executions(Arc::clone(service))
        .with_hard_ceiling(hard_ceiling);
    tool.run(request, context())
        .await
        .expect("the foreground deadline returns a resumable execution")
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_terminal_receipts_preserve_failures_derived_status_and_remote_authority() {
    use zuno_tools::shell::ExitPolicy;
    for (command, policy, purpose, outcome, authority) in [
        (
            "false | true",
            ExitPolicy::Pipefail,
            BackgroundExecutionPurpose::Command,
            ReceiptOutcome::Failed,
            ExitAuthority::Authoritative,
        ),
        (
            "false | true",
            ExitPolicy::Last,
            BackgroundExecutionPurpose::Command,
            ReceiptOutcome::Passed,
            ExitAuthority::Derived,
        ),
        (
            "printf observed",
            ExitPolicy::All,
            BackgroundExecutionPurpose::RemoteObserver,
            ReceiptOutcome::Unknown,
            ExitAuthority::Absent,
        ),
    ] {
        let workspace = tempfile::tempdir().expect("workspace");
        let directory = tempfile::tempdir().expect("process state");
        let service =
            Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
        let mut request = params(format!("cat release >/dev/null && {command}"), Some(10));
        request.exit_policy = Some(policy);
        request.background_purpose = purpose;
        let launched =
            launch_resumable_request(workspace.path(), &service, request, Duration::from_secs(30))
                .await;
        let id =
            BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
                .expect("execution id");
        std::fs::write(workspace.path().join("release"), b"finish").expect("release fixture");
        wait_for_task(&service, &id).await;
        let result = BackgroundTool::new(service)
            .run(foreground_read(BackgroundAction::Output, &id), context())
            .await
            .expect("receipt");
        let receipt = VerificationReceipt::from_metadata(&result.metadata)
            .expect("valid")
            .expect("present");
        assert_eq!(receipt.outcome, outcome, "{command}");
        assert_eq!(receipt.exit_authority, authority, "{command}");
        assert!(!receipt.proves_success());
    }
}

#[cfg(unix)]
struct ForegroundInterrupt(tokio_util::sync::CancellationToken);

#[cfg(unix)]
#[async_trait::async_trait]
impl zuno_tool::InterruptHandle for ForegroundInterrupt {
    fn is_set(&self) -> bool {
        self.0.is_cancelled()
    }

    async fn notified(&self) {
        self.0.cancelled().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn interrupting_a_resumed_foreground_wait_cancels_the_original_process() {
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
    let launched = launch_resumable_foreground(workspace.path(), &service).await;
    let id = BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
        .expect("execution id");
    let pid = service
        .get(&id)
        .expect("original process")
        .pid
        .expect("running pid");
    let interrupt = Arc::new(ForegroundInterrupt(
        tokio_util::sync::CancellationToken::new(),
    ));
    let ctx = ToolContext::new(
        "ses_timeout",
        "next_message",
        "next_call",
        "build",
        Arc::new(AllowAll),
        interrupt.clone(),
    );
    interrupt.0.cancel();
    let mut request = foreground_read(BackgroundAction::Wait, &id);
    request.timeout = Some(5_000);
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        BackgroundTool::new(Arc::clone(&service)).run(request, ctx),
    )
    .await
    .expect("prompt cancellation")
    .expect("partial result");
    let facts = &output.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    assert_eq!(facts["waitInterrupted"], true);
    assert_eq!(facts["execution"]["status"], "cancelled");
    let receipt = VerificationReceipt::from_metadata(&output.metadata)
        .expect("valid")
        .expect("receipt");
    assert_eq!(receipt.outcome, ReceiptOutcome::Unknown);
    assert_eq!(receipt.exit_authority, ExitAuthority::Absent);
    assert_eq!(output.metadata["cancellation"]["uncertain"], true);
    assert_eq!(
        service.get(&id).expect("same handle").status,
        BackgroundExecutionStatus::Cancelled
    );
    wait_for_process_exit(pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_hard_ceiling_returns_an_unknown_receipt_without_replay_after_yield() {
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
    let launched = launch_resumable_request(
        workspace.path(),
        &service,
        params("printf launch >> launches && sleep 30", Some(10)),
        Duration::from_secs(1),
    )
    .await;
    let id = BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
        .expect("execution id");
    let settled = wait_for_task(&service, &id).await;
    assert!(settled.timed_out);
    let result = BackgroundTool::new(service)
        .run(foreground_read(BackgroundAction::Output, &id), context())
        .await
        .expect("receipt");
    let receipt = VerificationReceipt::from_metadata(&result.metadata)
        .expect("valid")
        .expect("receipt");
    assert_eq!(receipt.outcome, ReceiptOutcome::Unknown);
    assert_eq!(receipt.exit_authority, ExitAuthority::Absent);
    assert_eq!(
        std::fs::read(workspace.path().join("launches")).expect("single launch"),
        b"launch"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn foreground_drain_keeps_the_full_receipt_while_output_reads_stay_bounded() {
    use sha2::{Digest as _, Sha256};
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
    let request = params(
        "cat release >/dev/null && head -c 70000 /dev/zero && printf verified",
        Some(10),
    );
    let launched =
        launch_resumable_request(workspace.path(), &service, request, Duration::from_secs(30))
            .await;
    let id = BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
        .expect("execution id");
    std::fs::write(workspace.path().join("release"), b"finish").expect("release fixture");
    wait_for_task(&service, &id).await;
    let mut request = foreground_read(BackgroundAction::Output, &id);
    request.limit = Some(8);
    let bg = BackgroundTool::new(Arc::clone(&service));
    let window = bg.run(request, context()).await.expect("bounded read");
    let facts = &window.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY];
    assert_eq!(facts["cursor"], 8);
    assert_eq!(facts["hasMore"], true);
    let mut expected = vec![0; 70_000];
    expected.extend_from_slice(b"verified");
    let receipt = VerificationReceipt::from_metadata(&window.metadata)
        .expect("valid")
        .expect("receipt");
    assert_eq!(
        receipt.output_digest,
        Some(hex::encode(Sha256::digest(&expected)))
    );
    assert!(receipt.proves_success());
    let drained = service.drain_foreground("ses_timeout", None);
    assert_eq!(
        drained.len(),
        1,
        "a read without a durable sink does not acknowledge delivery"
    );
    let completion = drained
        .into_iter()
        .next()
        .expect("ready")
        .expect("captured result");
    let store = tempfile::tempdir().expect("host output store");
    let output = zuno_tools::shell::foreground_completion_output(
        &completion,
        zuno_tool::ToolOutputStore::new(store.path()),
        zuno_tool::OutputLimits {
            max_bytes: 16,
            max_lines: 10,
        },
    )
    .expect("host rendering");
    assert_eq!(output.metadata["origin_call_id"], "call_timeout");
    assert_eq!(
        VerificationReceipt::from_metadata(&output.metadata)
            .expect("valid")
            .expect("receipt"),
        receipt
    );
    let paths = output.output_paths();
    assert_eq!(paths.len(), 1);
    assert_eq!(std::fs::read(paths[0]).expect("host artifact"), expected);
    service
        .consume_foreground(&id, "ses_timeout")
        .expect("acknowledge after host persistence");
    let mut tail = foreground_read(BackgroundAction::Output, &id);
    tail.cursor = Some(70_000);
    tail.limit = Some(8);
    let tail = bg
        .run(tail, context())
        .await
        .expect("page after acknowledgement");
    assert_eq!(
        tail.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY]["output"],
        "verified"
    );
    assert_eq!(
        tail.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY]["foregroundConsumed"],
        true
    );
}

#[cfg(unix)]
#[tokio::test]
async fn resumable_foreground_deadline_preserves_the_same_process_and_handle() {
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
    let launched = launch_resumable_foreground(workspace.path(), &service).await;
    let id = BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
        .expect("execution id");
    let started = service.get(&id).expect("same execution");
    let bg = BackgroundTool::new(Arc::clone(&service));
    let progress = bg
        .run(foreground_read(BackgroundAction::Wait, &id), context())
        .await
        .expect("bounded observation");
    let still_running = service.get(&id).expect("still owned");
    std::fs::write(workspace.path().join("release"), b"finish").expect("release fixture");
    let settled = wait_for_task(&service, &id).await;

    assert_eq!(
        started.pid, still_running.pid,
        "waiting cannot spawn another process"
    );
    assert_eq!(settled.id, id);
    assert_eq!(settled.status, BackgroundExecutionStatus::Completed);
    assert_eq!(
        progress.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY]["execution"]["taskID"],
        id.as_str()
    );
    assert_eq!(
        progress.metadata[zuno_tools::bg::BACKGROUND_METADATA_KEY]["waitTimedOut"],
        true
    );
    assert_eq!(
        std::fs::read(workspace.path().join("launches")).expect("one launch"),
        b"launch",
        "an observation timeout must never replay a side effect"
    );
    assert_eq!(
        launched.metadata["background"], false,
        "a deadline is not detachment"
    );
    assert_eq!(launched.metadata["foreground_yielded"], true);
    assert_ne!(
        launched.metadata.get("timeout_promoted"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn resumable_foreground_never_emits_detached_completion_callbacks() {
    let workspace = tempfile::tempdir().expect("workspace");
    let directory = tempfile::tempdir().expect("process state");
    let service = Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
    let mut callbacks = service.subscribe();
    let launched = launch_resumable_foreground(workspace.path(), &service).await;
    let id = BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
        .expect("execution id");
    std::fs::write(workspace.path().join("release"), b"finish").expect("release fixture");
    wait_for_task(&service, &id).await;

    assert!(
        callbacks.try_recv().is_err(),
        "yielding or settling foreground work must not publish a detached callback"
    );
    assert!(
        service.list_for_session("ses_timeout").is_empty(),
        "the detached replay scan must never rediscover foreground completions"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn resumable_foreground_terminal_reads_recover_the_shell_verification_receipt() {
    for action in [BackgroundAction::Wait, BackgroundAction::Output] {
        let workspace = tempfile::tempdir().expect("workspace");
        let directory = tempfile::tempdir().expect("process state");
        let service =
            Arc::new(BackgroundExecutionService::open(directory.path()).expect("service"));
        let launched = launch_resumable_foreground(workspace.path(), &service).await;
        let id =
            BackgroundExecutionId::parse(launched.metadata["task_id"].as_str().expect("handle"))
                .expect("execution id");
        std::fs::write(workspace.path().join("release"), b"finish").expect("release fixture");
        wait_for_task(&service, &id).await;
        let result = BackgroundTool::new(Arc::clone(&service))
            .run(foreground_read(action, &id), context())
            .await
            .expect("terminal read");
        let receipt = VerificationReceipt::from_metadata(&result.metadata)
            .expect("valid receipt")
            .expect("a yielded command must keep its exit authority");
        assert_eq!(receipt.exit_authority, ExitAuthority::Authoritative);
        assert_eq!(receipt.outcome, ReceiptOutcome::Passed);
        assert_eq!(receipt.exit_code, Some(0));
        assert_eq!(
            receipt.workdir.as_deref(),
            Some(workspace.path().to_str().expect("workspace path"))
        );
        assert!(receipt.output_digest.is_some());
        assert!(receipt.proves_success());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_policy_yields_a_live_process_as_a_reachable_foreground_handle() {
    let workspace = tempfile::tempdir().expect("workspace");
    let background_dir = tempfile::tempdir().expect("background dir");
    let service = Arc::new(
        BackgroundExecutionService::open(background_dir.path()).expect("background service"),
    );
    let pid_file = workspace.path().join("yielded.pid");
    let marker = workspace.path().join("yielded.done");
    let command = format!(
        "printf '%s' \"$$\" > '{}'; sleep 0.25; printf finished; touch '{}'",
        pid_file.display(),
        marker.display()
    );
    let tool =
        support::sandbox::shell_tool(workspace.path()).with_background_executions(service.clone());

    let output = tool
        .run(params(command, Some(40)), context())
        .await
        .expect("foreground timeout yields the existing handle");

    assert_eq!(output.metadata["background"], false);
    assert_eq!(output.metadata["foreground_yielded"], true);
    assert!(output.output.contains(
        "The command is still running; do not rerun it unless you intentionally want a second copy."
    ));
    let pid = wait_for_pid(&pid_file).await;
    assert!(process_exists(pid), "yielded process {pid} was killed");

    let task_id = output.metadata["task_id"]
        .as_str()
        .expect("task id metadata");
    let task_id = BackgroundExecutionId::parse(task_id).expect("valid task id");
    let initial = service.get(&task_id).expect("reachable foreground task");
    assert_eq!(initial.id, task_id);
    assert!(initial.output_file.exists());
    assert!(initial.status_file.exists());

    let completed = wait_for_task(&service, &task_id).await;
    assert_eq!(completed.status, BackgroundExecutionStatus::Completed);
    assert_eq!(
        String::from_utf8(service.complete_output(&task_id).expect("completed output"))
            .expect("UTF-8 output"),
        "finished",
    );
    assert_eq!(
        std::fs::read_to_string(completed.output_file).expect("background output file"),
        "finished"
    );
    assert!(marker.exists());
    wait_for_process_exit(pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_policy_hard_ceiling_still_terminates_the_process_group() {
    let workspace = tempfile::tempdir().expect("workspace");
    let background_dir = tempfile::tempdir().expect("background dir");
    let service = Arc::new(
        BackgroundExecutionService::open(background_dir.path()).expect("background service"),
    );
    let pid_file = workspace.path().join("ceiling.pid");
    let command = format!("printf '%s' \"$$\" > '{}'; sleep 30", pid_file.display());
    let tool = support::sandbox::shell_tool(workspace.path())
        .with_background_executions(service.clone())
        .with_hard_ceiling(Duration::from_secs(3));
    let started = Instant::now();

    // The foreground timeout tests yielding, while the much wider hard
    // ceiling tests eventual termination. Keeping those clocks separated
    // prevents hosted-runner scheduler latency from winning before the child
    // has written the PID that proves a real process was later reaped.
    let error = tool
        .run(params(command, Some(30)), context())
        .await
        .expect("foreground timeout first returns a task handle");

    let task_id = error.metadata["task_id"]
        .as_str()
        .expect("foreground task id");
    let task_id = BackgroundExecutionId::parse(task_id).expect("valid task id");
    let pid = wait_for_pid(&pid_file).await;
    let failed = wait_for_task(&service, &task_id).await;
    assert_eq!(failed.status, BackgroundExecutionStatus::Failed);
    assert!(failed.timed_out);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|message| message.contains("hard ceiling")),
        "{:?}",
        failed.error
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    wait_for_process_exit(pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn completed_foreground_commands_leave_no_background_records_or_files() {
    let workspace = tempfile::tempdir().expect("workspace");
    let background_dir = tempfile::tempdir().expect("background dir");
    let service = Arc::new(
        BackgroundExecutionService::open(background_dir.path()).expect("background service"),
    );
    let tool =
        support::sandbox::shell_tool(workspace.path()).with_background_executions(service.clone());

    let output = tool
        .run(params("printf foreground", Some(1_000)), context())
        .await
        .expect("foreground command");

    assert_eq!(output.output, "foreground");
    assert!(service.list().is_empty());
    assert_eq!(
        std::fs::read_dir(background_dir.path())
            .expect("background directory")
            .count(),
        0,
        "a completed foreground command must not leave durable execution artifacts"
    );
}

#[cfg(unix)]
async fn wait_for_task(
    service: &BackgroundExecutionService,
    task_id: &BackgroundExecutionId,
) -> zuno_pty::BackgroundExecutionInfo {
    tokio::time::timeout(Duration::from_secs(4), service.wait(task_id, None))
        .await
        .expect("background task must settle")
        .expect("registered task")
        .info
}

/// The child creates the pid file and writes to it as two separate steps, so waiting only for
/// the path to exist can observe a created-but-empty file and parse `""`.
#[cfg(unix)]
async fn wait_for_pid(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        if path.exists() {
            panic!(
                "{} never contained a numeric pid (last contents: {:?})",
                path.display(),
                std::fs::read_to_string(path).ok()
            )
        }
        panic!("{} was not created", path.display())
    })
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    std::path::PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} survived termination"));
}
