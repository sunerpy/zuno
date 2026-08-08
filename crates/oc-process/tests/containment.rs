#![cfg(target_os = "linux")]

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

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
    let child = Command::new(env!("CARGO_BIN_EXE_oc-process-fixture"))
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
