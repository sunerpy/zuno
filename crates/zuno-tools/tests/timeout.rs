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
#[tokio::test]
async fn timeout_policy_promotes_a_live_process_to_a_reachable_background_task() {
    let workspace = tempfile::tempdir().expect("workspace");
    let background_dir = tempfile::tempdir().expect("background dir");
    let service = Arc::new(
        BackgroundExecutionService::open(background_dir.path()).expect("background service"),
    );
    let pid_file = workspace.path().join("promoted.pid");
    let marker = workspace.path().join("promoted.done");
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
        .expect("foreground timeout promotes rather than fails");

    assert_eq!(output.metadata["background"], true);
    assert_eq!(output.metadata["timeout_promoted"], true);
    assert!(output.output.contains(
        "The command is still running; do not rerun it unless you intentionally want a second copy."
    ));
    let pid = wait_for_pid(&pid_file).await;
    assert!(process_exists(pid), "promoted process {pid} was killed");

    let task_id = output.metadata["task_id"]
        .as_str()
        .expect("task id metadata");
    let task_id = BackgroundExecutionId::parse(task_id).expect("valid task id");
    let initial = service.get(&task_id).expect("reachable promoted task");
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

    // The foreground timeout tests promotion, while the much wider hard
    // ceiling tests eventual termination. Keeping those clocks separated
    // prevents hosted-runner scheduler latency from winning before the child
    // has written the PID that proves a real process was later reaped.
    let error = tool
        .run(params(command, Some(30)), context())
        .await
        .expect("foreground timeout first returns a task handle");

    let task_id = error.metadata["task_id"]
        .as_str()
        .expect("promoted task id");
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
