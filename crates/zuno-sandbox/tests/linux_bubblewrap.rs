#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use zuno_sandbox::{
    LinuxBubblewrapSandbox, NetworkAccess, PrepareRequest, SandboxBackend, SandboxMode,
    SandboxPolicy,
};

fn e2e_helper() -> PathBuf {
    std::env::var_os("ZUNO_SANDBOX_E2E_HELPER")
        .map(PathBuf::from)
        .expect("set ZUNO_SANDBOX_E2E_HELPER to a built zuno executable")
}

fn run(
    backend: &LinuxBubblewrapSandbox,
    workspace: &Path,
    command: &str,
    mode: SandboxMode,
) -> std::process::Output {
    let prepared = backend
        .prepare(PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-lc"), OsString::from(command)],
            cwd: workspace.to_owned(),
            environment: BTreeMap::from([(
                OsString::from("PATH"),
                OsString::from("/usr/bin:/bin"),
            )]),
            policy: SandboxPolicy::new(workspace, mode, NetworkAccess::Denied)
                .expect("sandbox policy"),
        })
        .expect("prepared command");
    let parts = prepared.into_parts();
    let mut process = Command::new(parts.program);
    process
        .args(parts.arguments)
        .current_dir(parts.cwd)
        .env_clear()
        .envs(parts.environment);
    process.output().expect("sandboxed process")
}

#[test]
#[ignore = "requires host bubblewrap namespaces and a built Zuno helper"]
fn real_bwrap_enforces_filesystem_network_capability_and_syscall_boundaries() {
    let helper = e2e_helper();
    let root = tempfile::tempdir().expect("temporary E2E root");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&outside).expect("outside");
    for protected in [".git", ".zuno", ".agents"] {
        fs::create_dir(workspace.join(protected)).expect("protected directory");
    }
    std::os::unix::fs::symlink(&outside, workspace.join("outside-link")).expect("outside symlink");

    let backend =
        LinuxBubblewrapSandbox::discover_with_helper(&workspace, &helper).expect("Linux backend");
    let python = r#"
import ctypes, errno, os, socket

def expect_eperm(name, call):
    ctypes.set_errno(0)
    try:
        result = call()
    except OSError as exc:
        if exc.errno != errno.EPERM:
            raise SystemExit(f"{name}: expected EPERM, got {exc.errno}")
        return
    if result != -1 or ctypes.get_errno() != errno.EPERM:
        raise SystemExit(f"{name}: expected -1/EPERM, got {result}/{ctypes.get_errno()}")

expect_eperm("socket", lambda: socket.socket(socket.AF_INET, socket.SOCK_STREAM))
left, right = socket.socketpair()
os.write(left.fileno(), b"x")
if os.read(right.fileno(), 1) != b"x":
    raise SystemExit("AF_UNIX socketpair IPC failed")
left.close()
right.close()
libc = ctypes.CDLL(None, use_errno=True)
expect_eperm("ptrace", lambda: libc.ptrace(0, 0, 0, 0))
expect_eperm("process_vm_readv", lambda: libc.process_vm_readv(os.getpid(), 0, 0, 0, 0, 0))
"#;
    let command = format!(
        "set -eu\n\
         printf allowed > allowed.txt\n\
         ! printf blocked > .git/blocked 2>/dev/null\n\
         ! printf blocked > .zuno/blocked 2>/dev/null\n\
         ! printf blocked > .agents/blocked 2>/dev/null\n\
         ! printf blocked > {outside}/blocked 2>/dev/null\n\
         ! printf blocked > outside-link/blocked 2>/dev/null\n\
         grep -q '^NoNewPrivs:[[:space:]]*1$' /proc/self/status\n\
         grep -q '^CapEff:[[:space:]]*0000000000000000$' /proc/self/status\n\
         /usr/bin/python3 -c {python}",
        outside = shell_quote(&outside.to_string_lossy()),
        python = shell_quote(python),
    );
    let output = run(&backend, &workspace, &command, SandboxMode::WorkspaceWrite);

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(workspace.join("allowed.txt")).unwrap(),
        "allowed"
    );
    for blocked in [
        workspace.join(".git/blocked"),
        workspace.join(".zuno/blocked"),
        workspace.join(".agents/blocked"),
        outside.join("blocked"),
    ] {
        assert!(!blocked.exists(), "sandbox wrote {}", blocked.display());
    }

    let read_only = run(
        &backend,
        &workspace,
        "printf blocked > read-only-write 2>/dev/null",
        SandboxMode::ReadOnly,
    );
    assert!(
        !read_only.status.success(),
        "read-only policy allowed a write"
    );
    assert!(!workspace.join("read-only-write").exists());
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
