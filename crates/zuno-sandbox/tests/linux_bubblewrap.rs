#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};

use zuno_sandbox::{
    LinuxBubblewrapSandbox, NetworkAccess, PrepareRequest, SandboxBackend, SandboxError,
    SandboxMode, SandboxPolicy,
};

/// The probe-spawn counter is process-wide, so the tests that read it or spawn
/// probes run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Set by every gate that must produce native confinement evidence, currently
/// `make test-sandbox-e2e` in the CI and release-candidate Linux jobs. When it is
/// set, a prerequisite this suite cannot satisfy is a failure, so an unavailable
/// backend can never be mistaken for enforced boundaries. When it is unset, the
/// same situation is a skip with a named reason, which keeps developer hosts
/// without bubblewrap usable.
const REQUIRE_EVIDENCE: &str = "ZUNO_SANDBOX_E2E_REQUIRE";

/// Report a prerequisite the host does not provide. Returns so the caller can
/// stop; panics instead when the caller demanded real evidence.
fn skip(reason: &str) {
    assert!(
        std::env::var_os(REQUIRE_EVIDENCE).is_none(),
        "{REQUIRE_EVIDENCE} is set, so the bubblewrap boundary suite must run: {reason}"
    );
    eprintln!("skipping the bubblewrap boundary suite: {reason}");
}

fn e2e_helper() -> Result<PathBuf, String> {
    let Some(helper) = std::env::var_os("ZUNO_SANDBOX_E2E_HELPER").map(PathBuf::from) else {
        return Err(
            "ZUNO_SANDBOX_E2E_HELPER is unset; point it at a built zuno \
             executable, or run `make test-sandbox-e2e`"
                .to_owned(),
        );
    };
    if !helper.is_file() {
        return Err(format!(
            "ZUNO_SANDBOX_E2E_HELPER `{}` is not an executable file",
            helper.display()
        ));
    }
    Ok(helper)
}

/// The sandboxed program below runs `/bin/sh` and `/usr/bin/python3` by absolute
/// path, because the sandbox gets a fixed `PATH`. A host missing either one
/// cannot run the suite; that is a host gap, not a Zuno defect.
fn missing_sandboxed_interpreter() -> Option<String> {
    ["/bin/sh", "/usr/bin/python3"]
        .into_iter()
        .find(|path| !Path::new(path).is_file())
        .map(|path| format!("the sandboxed program needs `{path}`, which this host lacks"))
}

/// Classify a discovery failure exactly as the crate itself does: the six causes
/// that make `SandboxUnavailableCause::from_error` return a cause mean the host
/// cannot deploy bubblewrap, and every other variant is a defect that must fail.
fn unavailable_backend(error: &SandboxError) -> Option<String> {
    match error {
        SandboxError::UnsupportedPlatform(_)
        | SandboxError::UnsupportedArchitecture(_)
        | SandboxError::Wsl1Unsupported
        | SandboxError::BubblewrapNotFound
        | SandboxError::MissingBubblewrapCapability(_)
        | SandboxError::UnavailableCapability { .. } => Some(error.to_string()),
        SandboxError::UntrustedBubblewrap { .. }
        | SandboxError::ProbeFailed { .. }
        | SandboxError::UnsupportedPolicy { .. }
        | SandboxError::InvalidPolicy(_)
        | SandboxError::InvalidPath { .. }
        | SandboxError::Seccomp(_)
        | SandboxError::Helper(_)
        | SandboxError::Io(_) => None,
    }
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
fn real_bwrap_enforces_filesystem_network_capability_and_syscall_boundaries() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let helper = match e2e_helper() {
        Ok(helper) => helper,
        Err(reason) => return skip(&reason),
    };
    if let Some(reason) = missing_sandboxed_interpreter() {
        return skip(&reason);
    }
    let root = tempfile::tempdir().expect("temporary E2E root");
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    fs::create_dir(&workspace).expect("workspace");
    fs::create_dir(&outside).expect("outside");
    for protected in [".git", ".zuno", ".agents"] {
        fs::create_dir(workspace.join(protected)).expect("protected directory");
    }
    std::os::unix::fs::symlink(&outside, workspace.join("outside-link")).expect("outside symlink");

    let backend = match LinuxBubblewrapSandbox::discover_with_helper(&workspace, &helper) {
        Ok(backend) => backend,
        Err(error) => match unavailable_backend(&error) {
            Some(reason) => return skip(&reason),
            None => panic!("Linux backend discovery failed: {error}"),
        },
    };
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

#[test]
fn system_backend_discovery_is_cached_within_the_process() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let root = tempfile::tempdir().expect("temporary discovery root");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");

    // Given: one discovery for a workspace, which must probe bubblewrap.
    let before = zuno_sandbox::probe_spawn_count();
    let first = match zuno_sandbox::system_backend(&workspace, SandboxMode::WorkspaceWrite) {
        Ok(backend) => backend,
        Err(error) => match unavailable_backend(&error) {
            Some(reason) => return skip(&reason),
            None => panic!("Linux backend discovery failed: {error}"),
        },
    };
    let after_first = zuno_sandbox::probe_spawn_count();
    assert!(
        after_first > before,
        "the first discovery must probe bubblewrap ({before} -> {after_first})"
    );

    // When: the same workspace is discovered again in the same process.
    let second = zuno_sandbox::system_backend(&workspace, SandboxMode::WorkspaceWrite)
        .expect("a second discovery for the same workspace succeeds");

    // Then: the answer is the same and no bubblewrap process was spawned to get it.
    assert_eq!(
        zuno_sandbox::probe_spawn_count(),
        after_first,
        "a repeated discovery for the same inputs must be served from the process cache"
    );
    assert_eq!(first.capabilities(), second.capabilities());
}
