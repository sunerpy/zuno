#![cfg(target_os = "linux")]

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read as _, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn guarded_pty_payload_can_read_from_the_terminal() {
    let mut pty = GuardedPty::spawn("read x; printf 'READ:%s\\n' \"$x\"");
    pty.write(b"hello\n");

    let (status, output) = pty.finish();
    assert!(status.success(), "guarded PTY failed: {status}; {output:?}");
    assert!(
        output.contains("READ:hello"),
        "guarded payload did not receive terminal input: {output:?}"
    );
}

#[test]
fn terminal_ctrl_c_reaches_the_guarded_payload() {
    let mut pty = GuardedPty::spawn(
        "trap 'printf \\\"INTERRUPTED\\\\n\\\"; exit 42' INT; \
         printf 'READY\\n'; while :; do sleep 1; done",
    );
    assert!(
        pty.wait_for_output("READY"),
        "guarded payload never became ready: {:?}",
        pty.output()
    );

    pty.write(b"\x03");
    let (status, output) = pty.finish();
    assert_eq!(
        status.exit_code(),
        42,
        "terminal SIGINT did not preserve the payload's trap status: {output:?}"
    );
    assert!(
        output.contains("INTERRUPTED"),
        "terminal SIGINT did not reach the payload: {output:?}"
    );
}

#[test]
fn clean_parent_shutdown_reaps_the_guarded_process_tree() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let (mut parent, ready, stop) = spawn_parent(directory.path());
    wait_until_ready(&mut parent, &ready);
    let pids = collect_process_tree(parent.id()).expect("fixture process tree");
    assert!(
        pids.len() >= 5,
        "expected parent, guard, monitor, payload, and grandchild: {pids:?}"
    );

    fs::write(&stop, b"stop").expect("request clean fixture shutdown");
    let status = parent.wait().expect("wait for clean parent shutdown");
    assert!(status.success(), "fixture parent failed: {status}");
    assert_processes_exit(&pids);
}

#[test]
fn parent_sigkill_reaps_the_guarded_process_tree() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let (mut parent, ready, _stop) = spawn_parent(directory.path());
    wait_until_ready(&mut parent, &ready);
    let pids = collect_process_tree(parent.id()).expect("fixture process tree");
    assert!(
        pids.len() >= 5,
        "expected parent, guard, monitor, payload, and grandchild: {pids:?}"
    );

    let parent_pid =
        rustix::process::Pid::from_raw(parent.id() as i32).expect("non-zero parent PID");
    rustix::process::kill_process(parent_pid, rustix::process::Signal::KILL)
        .expect("SIGKILL fixture parent");
    let _status = parent.wait().expect("reap killed fixture parent");
    assert_processes_exit(&pids);
}

fn spawn_parent(directory: &Path) -> (Child, std::path::PathBuf, std::path::PathBuf) {
    let ready = directory.join("ready");
    let stop = directory.join("stop");
    let child = Command::new(env!("CARGO_BIN_EXE_zuno-process-fixture"))
        .arg("parent")
        .arg(&ready)
        .arg(&stop)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn fixture parent");
    (child, ready, stop)
}

fn wait_until_ready(parent: &mut Child, ready: &Path) {
    let started = Instant::now();
    loop {
        if ready.exists() {
            return;
        }
        if let Some(status) = parent.try_wait().expect("poll fixture parent") {
            panic!("fixture parent exited before ready: {status}");
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "fixture process tree did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn collect_process_tree(root: u32) -> io::Result<Vec<u32>> {
    let mut seen = BTreeSet::new();
    let mut pending = VecDeque::from([root]);
    while let Some(pid) = pending.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        let task = match fs::read_dir(format!("/proc/{pid}/task")) {
            Ok(task) => task,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for thread in task {
            let children = fs::read_to_string(thread?.path().join("children"))?;
            for child in children.split_whitespace() {
                pending.push_back(child.parse().map_err(io::Error::other)?);
            }
        }
    }
    Ok(seen.into_iter().collect())
}

fn assert_processes_exit(pids: &[u32]) {
    let started = Instant::now();
    loop {
        let remaining: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|pid| Path::new(&format!("/proc/{pid}")).exists())
            .collect();
        if remaining.is_empty() {
            return;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "fixture PIDs were not reaped: {remaining:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct GuardedPty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Option<Box<dyn Write + Send>>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<io::Result<()>>>,
}

impl GuardedPty {
    fn spawn(script: &str) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open fixture PTY");
        let parent_pid = std::process::id().to_string();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_zuno-process-fixture"));
        command.args([
            "__oc_child_guard",
            "supervise",
            &parent_pid,
            "--",
            "/bin/sh",
            "-c",
            script,
        ]);
        let child = pair
            .slave
            .spawn_command(command)
            .expect("spawn guarded PTY payload");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let writer = pair.master.take_writer().expect("take PTY writer");
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader_output = Arc::clone(&output);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return Ok(()),
                    Ok(read) => reader_output
                        .lock()
                        .expect("PTY output lock")
                        .extend_from_slice(&buffer[..read]),
                    Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        });

        Self {
            child,
            writer: Some(writer),
            output,
            reader: Some(reader),
        }
    }

    fn write(&mut self, input: &[u8]) {
        let writer = self.writer.as_mut().expect("PTY writer is open");
        writer.write_all(input).expect("write PTY input");
        writer.flush().expect("flush PTY input");
    }

    fn wait_for_output(&mut self, expected: &str) -> bool {
        let started = Instant::now();
        while started.elapsed() < TIMEOUT {
            if self.output().contains(expected) {
                return true;
            }
            if self.child.try_wait().expect("poll guarded PTY").is_some() {
                return self.output().contains(expected);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn finish(&mut self) -> (portable_pty::ExitStatus, String) {
        let started = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("poll guarded PTY") {
                self.writer.take();
                self.join_reader();
                return (status, self.output());
            }
            if started.elapsed() >= TIMEOUT {
                let output = self.output();
                self.stop();
                panic!("guarded PTY did not exit within {TIMEOUT:?}: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().expect("PTY output lock")).into_owned()
    }

    fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .expect("PTY reader thread panicked")
                .expect("read PTY output");
        }
    }

    fn stop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _result = self.child.kill();
        }
        let _result = self.child.wait();
        self.writer.take();
        self.join_reader();
    }
}

impl Drop for GuardedPty {
    fn drop(&mut self) {
        self.stop();
    }
}
