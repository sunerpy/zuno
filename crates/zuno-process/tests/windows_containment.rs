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

fn wait_for_descendant_pid(ready: &Path) -> u32 {
    let started = Instant::now();
    let mut last_contents = None;
    loop {
        if let Ok(contents) = fs::read_to_string(ready) {
            let mut ids = contents.split_whitespace();
            if let (Some(parent), Some(descendant), None) = (ids.next(), ids.next(), ids.next())
                && parent.parse::<u32>().is_ok()
                && let Ok(descendant) = descendant.parse::<u32>()
            {
                return descendant;
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
