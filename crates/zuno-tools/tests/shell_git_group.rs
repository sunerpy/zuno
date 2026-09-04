//! The teardown of an expired pre-flight `git` read, with the process guard active.
//!
//! Its own test binary because [`zuno_process::activate_guard_executable`] sets a
//! process-global `OnceLock`: production activates one at startup
//! (`crates/zuno-cli/src/main.rs`), so a teardown that behaves differently with a guard
//! active behaves differently in production than in any test that forgot to activate one.
//! Activating it inside `tests/shell.rs` would instead reroute every background execution
//! in that binary through the guard argv, so the two cannot share a process.

mod support;

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use zuno_error::ToolError;
#[cfg(unix)]
use zuno_pty::BackgroundExecutionPurpose;
#[cfg(unix)]
use zuno_tool::{AllowAll, NeverInterrupted, ToolContext};
#[cfg(unix)]
use zuno_tools::shell::{ShellEnvHook, ShellEnvInput, ShellParams};

#[cfg(unix)]
struct OnlyFakeGitOnPath {
    bin: String,
}

#[cfg(unix)]
#[async_trait]
impl ShellEnvHook for OnlyFakeGitOnPath {
    async fn env(&self, _input: ShellEnvInput) -> Result<BTreeMap<String, String>, ToolError> {
        Ok(BTreeMap::from([("PATH".to_owned(), self.bin.clone())]))
    }
}

#[cfg(unix)]
fn write_fake_git(bin: &Path, body: &str) {
    let path = bin.join("git");
    std::fs::write(&path, format!("#!/bin/sh\nPATH=/usr/bin:/bin\n{body}"))
        .expect("write the fake git");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the fake git executable");
}

#[cfg(unix)]
async fn read_pid_when_written(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(raw) = std::fs::read_to_string(path)
                && let Ok(pid) = raw.trim().parse::<u32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} was never written", path.display()))
}

/// Whether `pid` still names a process, reaped or not.
///
/// `ps -p` rather than `/proc/{pid}`, which exists only on Linux and would make this
/// assertion vacuously true on macOS.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The probe itself has to be able to see a live process.
///
/// `ps` missing, or refusing, would make [`process_exists`] answer `false` for every pid,
/// and every "it is gone" assertion below would pass without having observed anything —
/// the same vacuous shape as probing `/proc` on macOS. This process's own pid is the one
/// case whose answer is known.
#[cfg(unix)]
fn assert_process_probe_works() {
    assert!(
        process_exists(std::process::id()),
        "`ps -p` cannot see this test process, so it cannot witness any other process \
         either: the exit assertions would be vacuous"
    );
}

#[cfg(unix)]
async fn assert_process_stopped(label: &str, pid: u32) {
    assert_process_probe_works();
    let stopped = tokio::time::timeout(Duration::from_secs(2), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "{label} (pid {pid}) outlived the refusal: the expired read's process group was \
         never torn down"
    );
}

#[cfg(unix)]
fn params(command: impl Into<String>) -> ShellParams {
    ShellParams {
        command: command.into(),
        timeout: None,
        workdir: None,
        background: false,
        background_purpose: BackgroundExecutionPurpose::Command,
        expected_git_head: None,
        exit_policy: None,
    }
}

#[cfg(unix)]
fn context() -> ToolContext {
    ToolContext::new(
        "ses_shell_git_group",
        "msg_shell_git_group",
        "call_shell_git_group",
        "build",
        Arc::new(AllowAll),
        Arc::new(NeverInterrupted),
    )
}

/// An expired pre-flight read takes everything git started with it, guard or no guard.
///
/// `kill_on_drop` reaches the direct child only. What used to reach the rest was
/// `zuno_process::request_contained_process_shutdown`, and that helper is for a child
/// launched through `guarded_argv`: with a guard active — every production Unix Zuno — it
/// sends one `SIGTERM` to the pid and trusts the guard to stop and reap the group. No guard
/// owns this read, and the pid it aimed at had already been `SIGKILL`ed by `kill_on_drop`,
/// so a credential helper, `fsmonitor`, a `textconv` filter or an `ssh` git had spawned
/// stayed alive as an orphan with nothing left to reap it.
#[cfg(unix)]
#[tokio::test]
async fn an_expired_pre_flight_read_stops_the_helpers_git_spawned() {
    zuno_process::activate_guard_executable(std::env::current_exe().expect("this test binary"))
        .expect("activate the process guard for this binary");

    let workspace = tempfile::tempdir().expect("temporary workspace");
    let bin = tempfile::tempdir().expect("fake git directory");
    let leader_pid = workspace.path().join("git.pid");
    let helper_pid = workspace.path().join("helper.pid");
    // The helper is a child of the read, so it joins the group `process_group(0)` gave the
    // read. `exec` so the leader pid is the process that actually outlives the ceiling.
    write_fake_git(
        bin.path(),
        &format!(
            "sleep 60 &\nprintf '%s' \"$!\" > '{}'\nprintf '%s' \"$$\" > '{}'\nexec sleep 60\n",
            helper_pid.display(),
            leader_pid.display()
        ),
    );
    let tool = support::sandbox::shell_tool(workspace.path())
        .with_env_hook(Arc::new(OnlyFakeGitOnPath {
            bin: bin.path().display().to_string(),
        }))
        .with_git_ceiling(Duration::from_millis(300));

    let leader = read_pid_when_written(&leader_pid);
    let helper = read_pid_when_written(&helper_pid);
    let refusal = tokio::time::timeout(
        Duration::from_secs(10),
        tool.run(params("git commit --quiet -m wip"), context()),
    );
    let (leader, helper, refusal) = tokio::join!(leader, helper, refusal);
    let error = refusal
        .expect("an unanswered pre-flight read must settle at its ceiling")
        .expect_err("an unknown repository state must not admit the commit");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("did not answer within the 300ms"),
        "{rendered}"
    );

    assert_process_stopped("the expired read", leader).await;
    assert_process_stopped("the helper the read spawned", helper).await;
}
