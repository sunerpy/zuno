#![cfg(windows)]

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn conpty_launches_the_terminal_program_directly_after_guard_activation() {
    let executable = env!("CARGO_BIN_EXE_zuno-process-fixture");
    zuno_process::activate_guard_executable(executable).expect("activate fixture guard");

    let (program, arguments) = zuno_process::guarded_terminal_argv("cmd.exe", ["/Q", "/D"]);

    assert_eq!(program, "cmd.exe");
    assert_eq!(arguments, vec![OsString::from("/Q"), OsString::from("/D")]);
}

#[test]
fn top_level_exit_terminates_a_live_job_descendant() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let ready = directory.path().join("ready");
    let executable = env!("CARGO_BIN_EXE_zuno-process-fixture");
    zuno_process::activate_guard_executable(executable).expect("activate fixture guard");
    let (program, arguments) =
        zuno_process::guarded_argv(executable, ["exiting-payload".as_ref(), ready.as_os_str()]);
    let mut guard = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn guarded exiting payload");

    let status = guard.wait().expect("wait for guard");
    assert!(status.success(), "guard failed: {status}");
    let descendant = wait_for_descendant_pid(&ready);
    let cleanup = DescendantCleanup(descendant);
    assert_process_exits(descendant);
    std::mem::forget(cleanup);
}

#[test]
fn parent_exit_terminates_the_guarded_payload_tree() {
    // The parent launches guard + payload, waits for the payload to publish its PIDs,
    // then exits without stopping anything.
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let ready = directory.path().join("ready");
    let executable = env!("CARGO_BIN_EXE_zuno-process-fixture");
    let status = Command::new(executable)
        .arg("dying-parent")
        .arg(&ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("dying parent runs");
    assert!(status.success(), "dying parent failed: {status}");

    // The guard's parent watch holds a handle to the parent process object, so its
    // exit is observed even if the PID is reused, and the job is torn down.
    let (payload, descendant) = wait_for_pids(&ready);
    let cleanup = (DescendantCleanup(payload), DescendantCleanup(descendant));
    assert_process_exits(payload);
    assert_process_exits(descendant);
    std::mem::forget(cleanup);
}

#[test]
fn a_lost_parent_watch_helper_never_kills_a_healthy_payload() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let ready = directory.path().join("ready");
    let executable = env!("CARGO_BIN_EXE_zuno-process-fixture");
    zuno_process::activate_guard_executable(executable).expect("activate fixture guard");
    let (program, arguments) =
        zuno_process::guarded_argv(executable, ["payload".as_ref(), ready.as_os_str()]);
    let mut guard = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn guarded payload");
    let (payload, descendant) = wait_for_pids(&ready);
    let cleanup = (DescendantCleanup(payload), DescendantCleanup(descendant));

    // Kill the helper through which the guard watches this test process. That is
    // exactly the failure `tasklist` used to turn into "the parent is dead".
    let helper = wait_for_parent_watch_helper(guard.id());
    let killed = Command::new("taskkill")
        .args(["/PID", &helper.to_string(), "/F"])
        .output()
        .expect("taskkill runs");
    assert!(killed.status.success(), "taskkill failed: {killed:?}");
    std::thread::sleep(Duration::from_millis(1500));

    assert!(
        guard.try_wait().expect("observe guard").is_none(),
        "the guard must keep supervising the payload after losing its parent watch"
    );
    assert!(
        windows_process_exists(payload),
        "a lost parent watch must never be read as a dead parent"
    );

    // The supported shutdown path still tears the tree down.
    zuno_process::request_contained_process_shutdown(guard.id()).expect("shutdown request");
    let _status = guard.wait();
    assert_process_exits(payload);
    assert_process_exits(descendant);
    std::mem::forget(cleanup);
}

fn wait_for_parent_watch_helper(guard_pid: u32) -> u32 {
    let started = Instant::now();
    loop {
        let query = format!(
            "(Get-CimInstance Win32_Process -Filter \"ParentProcessId = {guard_pid} and Name = 'powershell.exe'\").ProcessId"
        );
        let output = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &query])
            .output()
            .expect("powershell runs");
        if let Some(pid) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.trim().parse::<u32>().ok())
        {
            return pid;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "the guard did not start its parent-watch helper: {output:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_descendant_pid(ready: &Path) -> u32 {
    wait_for_pids(ready).1
}

/// The `(payload, grandchild)` PIDs the fixture payload publishes once both exist.
fn wait_for_pids(ready: &Path) -> (u32, u32) {
    let started = Instant::now();
    let mut last_contents = None;
    loop {
        if let Ok(contents) = fs::read_to_string(ready) {
            let mut ids = contents.split_whitespace();
            if let (Some(payload), Some(descendant), None) = (ids.next(), ids.next(), ids.next())
                && let Ok(payload) = payload.parse::<u32>()
                && let Ok(descendant) = descendant.parse::<u32>()
            {
                return (payload, descendant);
            }
            last_contents = Some(contents);
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "payload did not publish a complete descendant PID; last contents: {last_contents:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_process_exits(pid: u32) {
    let started = Instant::now();
    while windows_process_exists(pid) {
        assert!(
            started.elapsed() < TIMEOUT,
            "job descendant {pid} remained alive after its top-level process exited"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn windows_process_exists(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        })
}

struct DescendantCleanup(u32);

impl Drop for DescendantCleanup {
    fn drop(&mut self) {
        if windows_process_exists(self.0) {
            let _output = Command::new("taskkill")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .output();
        }
    }
}
