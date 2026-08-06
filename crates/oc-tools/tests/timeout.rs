use oc_tool::{AllowAll, NeverInterrupted, ToolContext};
use oc_tools::shell::{ShellParams, ShellTool};
use oc_tools::timeout::{
    BackgroundManager, BackgroundTaskStatus, LocalBackgroundManager, MAX_FOREGROUND_TIMEOUT_MS,
    normalize_foreground_timeout,
};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn params(command: impl Into<String>, timeout: Option<u64>) -> ShellParams {
    ShellParams {
        command: command.into(),
        timeout,
        workdir: None,
        background: false,
        justification: None,
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
    let manager = Arc::new(LocalBackgroundManager::new(background_dir.path()));
    let pid_file = workspace.path().join("promoted.pid");
    let marker = workspace.path().join("promoted.done");
    let command = format!(
        "printf '%s' \"$$\" > '{}'; sleep 0.25; printf finished; touch '{}'",
        pid_file.display(),
        marker.display()
    );
    let tool = ShellTool::new(workspace.path())
        .expect("shell tool")
        .with_background_manager(manager.clone());

    let output = tool
        .run(params(command, Some(40)), context())
        .await
        .expect("foreground timeout promotes rather than fails");

    assert_eq!(output.metadata["background"], true);
    assert_eq!(output.metadata["timeout_promoted"], true);
    assert!(output.output.contains(
        "The command is still running; do not rerun it unless you intentionally want a second copy."
    ));
    wait_for_file(&pid_file).await;
    let pid = read_pid(&pid_file);
    assert!(process_exists(pid), "promoted process {pid} was killed");

    let task_id = output.metadata["task_id"]
        .as_str()
        .expect("task id metadata");
    let initial = manager.task(task_id).expect("reachable promoted task");
    assert_eq!(initial.handle.task_id, task_id);
    assert!(initial.handle.output_file.exists());
    assert!(initial.handle.status_file.exists());

    let completed = wait_for_task(&manager, task_id).await;
    assert_eq!(completed.status, BackgroundTaskStatus::Completed);
    assert_eq!(
        completed.result.expect("completed output").output,
        "finished"
    );
    assert_eq!(
        std::fs::read_to_string(completed.handle.output_file).expect("background output file"),
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
    let manager = Arc::new(LocalBackgroundManager::new(background_dir.path()));
    let pid_file = workspace.path().join("ceiling.pid");
    let command = format!("printf '%s' \"$$\" > '{}'; sleep 30", pid_file.display());
    let tool = ShellTool::new(workspace.path())
        .expect("shell tool")
        .with_background_manager(manager.clone())
        .with_hard_ceiling(Duration::from_millis(120));
    let started = Instant::now();

    let error = tool
        .run(params(command, Some(30)), context())
        .await
        .expect("foreground timeout first returns a task handle");

    let task_id = error.metadata["task_id"]
        .as_str()
        .expect("promoted task id");
    wait_for_file(&pid_file).await;
    let pid = read_pid(&pid_file);
    let failed = wait_for_task(&manager, task_id).await;
    assert_eq!(failed.status, BackgroundTaskStatus::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|message| message.contains("timed out")),
        "{:?}",
        failed.error
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    wait_for_process_exit(pid).await;
}

#[cfg(unix)]
async fn wait_for_task(
    manager: &LocalBackgroundManager,
    task_id: &str,
) -> oc_tools::timeout::BackgroundTaskSnapshot {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = manager.task(task_id).expect("registered task");
            if snapshot.status != BackgroundTaskStatus::Running {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background task must settle")
}

#[cfg(unix)]
async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} was not created", path.display()));
}

#[cfg(unix)]
fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("pid file")
        .parse()
        .expect("numeric pid")
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
