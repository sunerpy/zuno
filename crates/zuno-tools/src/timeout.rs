//! Foreground deadlines hand attention back to the caller without killing work.
//!
//! Tokio documents `&mut JoinHandle<T>` as cancel-safe: timing out that borrowed
//! await does not abort the spawned task. The still-owned handle can therefore be
//! adopted by a background manager, matching jcode's handoff at
//! `.omo/refs/jcode/crates/jcode-app-core/src/tool/bash.rs:768-780,885-937`.
//! jcode's 600-second value caps the caller's foreground attention deadline; it is
//! not a process-lifetime ceiling. The shell keeps its existing 30-minute hard
//! ceiling, after which cancellation terminates the process group.

use serde_json::json;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::task::JoinHandle;
use zuno_error::ToolError;
use zuno_tool::ToolOutput;

pub const DEFAULT_FOREGROUND_TIMEOUT_MS: u64 = 120_000;
pub const MAX_FOREGROUND_TIMEOUT_MS: u64 = 600_000;
pub const DEFAULT_HARD_CEILING: Duration = Duration::from_secs(30 * 60);

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

pub type BackgroundWork = JoinHandle<Result<ToolOutput, ToolError>>;

pub struct BackgroundAdoption {
    pub tool_name: String,
    pub display_name: String,
    pub session_id: String,
    pub work: BackgroundWork,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTaskHandle {
    pub task_id: String,
    pub display_name: String,
    pub output_file: PathBuf,
    pub status_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Running,
    Completed,
    Failed,
}

impl BackgroundTaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTaskSnapshot {
    pub handle: BackgroundTaskHandle,
    pub tool_name: String,
    pub session_id: String,
    pub status: BackgroundTaskStatus,
    pub result: Option<ToolOutput>,
    pub error: Option<String>,
}

pub trait BackgroundManager: Send + Sync {
    /// Takes ownership of an already-running task; implementations must never abort it.
    fn adopt(&self, adoption: BackgroundAdoption) -> Result<BackgroundTaskHandle, ToolError>;

    fn task(&self, task_id: &str) -> Option<BackgroundTaskSnapshot>;
}

pub struct LocalBackgroundManager {
    root: PathBuf,
    tasks: Arc<Mutex<BTreeMap<String, BackgroundTaskSnapshot>>>,
}

impl LocalBackgroundManager {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn tasks(&self) -> MutexGuard<'_, BTreeMap<String, BackgroundTaskSnapshot>> {
        self.tasks.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn task_id(&self) -> String {
        let sequence = NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed);
        format!("bg_{:x}_{sequence:x}", std::process::id())
    }
}

impl BackgroundManager for LocalBackgroundManager {
    fn adopt(&self, adoption: BackgroundAdoption) -> Result<BackgroundTaskHandle, ToolError> {
        let BackgroundAdoption {
            tool_name,
            display_name,
            session_id,
            work,
        } = adoption;
        std::fs::create_dir_all(&self.root).map_err(|error| background_error(&tool_name, error))?;

        let task_id = self.task_id();
        let handle = BackgroundTaskHandle {
            output_file: self.root.join(format!("{task_id}.output")),
            status_file: self.root.join(format!("{task_id}.status.json")),
            task_id,
            display_name,
        };
        std::fs::write(&handle.output_file, [])
            .map_err(|error| background_error(&tool_name, error))?;

        let snapshot = BackgroundTaskSnapshot {
            handle: handle.clone(),
            tool_name: tool_name.clone(),
            session_id: session_id.clone(),
            status: BackgroundTaskStatus::Running,
            result: None,
            error: None,
        };
        write_status(&snapshot).map_err(|error| background_error(&tool_name, error))?;
        self.tasks()
            .insert(handle.task_id.clone(), snapshot.clone());

        let tasks = Arc::clone(&self.tasks);
        let _monitor = tokio::spawn(async move {
            let mut completed = snapshot;
            match work.await {
                Ok(Ok(output)) => {
                    match tokio::fs::write(&completed.handle.output_file, &output.output).await {
                        Ok(()) => {
                            completed.status = BackgroundTaskStatus::Completed;
                            completed.result = Some(output);
                        }
                        Err(error) => {
                            completed.status = BackgroundTaskStatus::Failed;
                            completed.error =
                                Some(format!("could not persist background output: {error}"));
                        }
                    }
                }
                Ok(Err(error)) => {
                    completed.status = BackgroundTaskStatus::Failed;
                    completed.error = Some(error.to_string());
                }
                Err(error) => {
                    completed.status = BackgroundTaskStatus::Failed;
                    completed.error = Some(format!("background task failed to join: {error}"));
                }
            }

            let status_bytes = status_json(&completed);
            let status_file = completed.handle.status_file.clone();
            let task_id = completed.handle.task_id.clone();
            tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(task_id, completed);
            let _status_write = tokio::fs::write(status_file, status_bytes).await;
        });

        Ok(handle)
    }

    fn task(&self, task_id: &str) -> Option<BackgroundTaskSnapshot> {
        self.tasks().get(task_id).cloned()
    }
}

pub struct ForegroundTask {
    pub tool_name: String,
    pub display_name: String,
    pub session_id: String,
    pub foreground_timeout_ms: u64,
    pub hard_ceiling: Duration,
    pub work: BackgroundWork,
}

pub async fn wait_or_promote(
    manager: &dyn BackgroundManager,
    task: ForegroundTask,
) -> Result<ToolOutput, ToolError> {
    let ForegroundTask {
        tool_name,
        display_name,
        session_id,
        foreground_timeout_ms,
        hard_ceiling,
        mut work,
    } = task;
    let foreground_timeout = Duration::from_millis(foreground_timeout_ms);

    if foreground_timeout >= hard_ceiling {
        return join_work(&tool_name, work).await;
    }

    match tokio::time::timeout(foreground_timeout, &mut work).await {
        Ok(joined) => flatten_join(&tool_name, joined),
        Err(_) => {
            let handle = manager.adopt(BackgroundAdoption {
                tool_name,
                display_name: display_name.clone(),
                session_id,
                work,
            })?;
            Ok(timeout_promoted_output(
                display_name,
                foreground_timeout_ms,
                handle,
            ))
        }
    }
}

#[must_use]
pub fn normalize_foreground_timeout(requested_ms: Option<u64>) -> u64 {
    requested_ms
        .unwrap_or(DEFAULT_FOREGROUND_TIMEOUT_MS)
        .min(MAX_FOREGROUND_TIMEOUT_MS)
}

#[must_use]
pub fn timeout_promoted_output(
    display_name: String,
    timeout_ms: u64,
    handle: BackgroundTaskHandle,
) -> ToolOutput {
    let output = format!(
        "Command exceeded the foreground timeout after {:.1}s and is continuing in background (not killed).\n\n\
         Task ID: {}\n\
         Name: {}\n\
         Output file: {}\n\
         Status file: {}\n\n\
         The command is still running; do not rerun it unless you intentionally want a second copy.\n\
         Use `bg` with action=\"wait\" and task_id=\"{}\" to wait for completion or the next progress checkpoint.\n\
         Use `bg` with action=\"output\" and task_id=\"{}\" to inspect output.\n\
         If you expected it to finish quickly and it did not, the `timeout` parameter is in MILLISECONDS; pass a larger value or omit it.",
        timeout_ms as f64 / 1000.0,
        handle.task_id,
        display_name,
        handle.output_file.display(),
        handle.status_file.display(),
        handle.task_id,
        handle.task_id,
    );

    ToolOutput::text(format!("{display_name} running in background"), output)
        .with_metadata("background", true)
        .with_metadata("task_id", handle.task_id)
        .with_metadata("display_name", display_name)
        .with_metadata(
            "output_file",
            handle.output_file.to_string_lossy().into_owned(),
        )
        .with_metadata(
            "status_file",
            handle.status_file.to_string_lossy().into_owned(),
        )
        .with_metadata("timeout_promoted", true)
        .with_metadata("foreground_timeout_ms", json!(timeout_ms))
}

#[must_use]
pub fn background_started_output(
    display_name: String,
    pid: Option<u32>,
    handle: BackgroundTaskHandle,
) -> ToolOutput {
    let output = format!(
        "Command is running in the background{}.\n\n\
         Task ID: {}\n\
         Name: {}\n\
         Output file: {}\n\
         Status file: {}",
        pid.map_or_else(String::new, |pid| format!(" with process id {pid}")),
        handle.task_id,
        display_name,
        handle.output_file.display(),
        handle.status_file.display(),
    );

    ToolOutput::text(format!("{display_name} running in background"), output)
        .with_metadata("background", true)
        .with_metadata("pid", json!(pid))
        .with_metadata("task_id", handle.task_id)
        .with_metadata("display_name", display_name)
        .with_metadata(
            "output_file",
            handle.output_file.to_string_lossy().into_owned(),
        )
        .with_metadata(
            "status_file",
            handle.status_file.to_string_lossy().into_owned(),
        )
}

async fn join_work(tool_name: &str, work: BackgroundWork) -> Result<ToolOutput, ToolError> {
    flatten_join(tool_name, work.await)
}

fn flatten_join(
    tool_name: &str,
    joined: Result<Result<ToolOutput, ToolError>, tokio::task::JoinError>,
) -> Result<ToolOutput, ToolError> {
    joined.map_err(|error| background_error(tool_name, io::Error::other(error)))?
}

fn write_status(snapshot: &BackgroundTaskSnapshot) -> io::Result<()> {
    std::fs::write(&snapshot.handle.status_file, status_json(snapshot))
}

fn status_json(snapshot: &BackgroundTaskSnapshot) -> Vec<u8> {
    serde_json::to_vec_pretty(&json!({
        "task_id": snapshot.handle.task_id,
        "tool_name": snapshot.tool_name,
        "display_name": snapshot.handle.display_name,
        "session_id": snapshot.session_id,
        "status": snapshot.status.as_str(),
        "error": snapshot.error,
        "output_file": snapshot.handle.output_file,
    }))
    .unwrap_or_else(|error| format!("{{\"status\":\"failed\",\"error\":{error:?}}}").into_bytes())
}

fn background_error(tool: &str, error: io::Error) -> ToolError {
    ToolError::Failed {
        tool: tool.to_owned(),
        source: Box::new(error),
    }
}
