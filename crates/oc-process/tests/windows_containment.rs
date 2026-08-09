#![cfg(windows)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn top_level_exit_terminates_a_live_job_descendant() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let ready = directory.path().join("ready");
    let executable = env!("CARGO_BIN_EXE_oc-process-fixture");
    oc_process::activate_guard_executable(executable).expect("activate fixture guard");
    let (program, arguments) =
        oc_process::guarded_argv(executable, ["exiting-payload".as_ref(), ready.as_os_str()]);
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

fn wait_for_descendant_pid(ready: &Path) -> u32 {
    let started = Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(ready) {
            return contents.trim().parse().expect("descendant PID");
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "payload did not publish its descendant PID"
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
