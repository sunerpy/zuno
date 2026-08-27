use crate::{
    HELPER_MARKER, NetworkAccess, PrepareRequest, PreparedCommand, SandboxBackend,
    SandboxCapabilities, SandboxError, SandboxMode,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const BACKEND_NAME: &str = "linux_bubblewrap";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const TRUSTED_BWRAP_CANDIDATES: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap"];
const TRUSTED_TRUE_CANDIDATES: &[&str] = &["/usr/bin/true", "/bin/true"];
const REQUIRED_BWRAP_OPTIONS: &[&str] = &[
    "--assert-userns-disabled",
    "--cap-drop",
    "--dev",
    "--die-with-parent",
    "--disable-userns",
    "--new-session",
    "--proc",
    "--ro-bind",
    "--unshare-ipc",
    "--unshare-net",
    "--unshare-pid",
    "--unshare-user",
    "--unshare-uts",
];

/// Linux bubblewrap plus in-sandbox seccomp backend.
#[derive(Debug)]
pub struct LinuxBubblewrapSandbox {
    workspace: PathBuf,
    current_exe: PathBuf,
    capabilities: SandboxCapabilities,
    network_probe_error: Option<String>,
}

impl LinuxBubblewrapSandbox {
    /// Discovers trusted host executables and probes every advertised namespace.
    pub fn discover(workspace: &Path) -> Result<Self, SandboxError> {
        let current_exe = std::env::current_exe()?;
        Self::discover_with_helper(workspace, &current_exe)
    }

    /// Discovers bubblewrap while using an explicit first-party helper executable.
    ///
    /// Embedders use this when the process that prepares a command is not the
    /// executable that handles [`HELPER_MARKER`]. The helper is canonicalized,
    /// required to be executable, and rebound read-only inside every sandbox.
    pub fn discover_with_helper(
        workspace: &Path,
        helper_executable: &Path,
    ) -> Result<Self, SandboxError> {
        reject_wsl1()?;
        let _architecture = target_arch()?;
        let workspace = canonical_directory(workspace, "workspace")?;
        let bwrap = trusted_executable(TRUSTED_BWRAP_CANDIDATES, &workspace, true)?;
        require_bwrap_options(&bwrap)?;
        let true_executable = trusted_executable(TRUSTED_TRUE_CANDIDATES, &workspace, false)?;
        let current_exe = validated_helper(helper_executable)?;

        run_probe(&bwrap, &true_executable, false).map_err(|detail| SandboxError::ProbeFailed {
            capability: "user, mount, PID, UTS, and IPC namespaces",
            detail,
        })?;
        let network_probe_error = run_probe(&bwrap, &true_executable, true).err();
        compile_seccomp(NetworkAccess::Allowed)?;

        Ok(Self {
            workspace,
            current_exe,
            capabilities: SandboxCapabilities {
                backend: BACKEND_NAME.to_owned(),
                executable: Some(bwrap),
                read_only: true,
                workspace_write: true,
                danger_full_access: false,
                network_isolation: network_probe_error.is_none(),
            },
            network_probe_error,
        })
    }

    fn prepare_inner(&self, mut request: PrepareRequest) -> Result<PreparedCommand, SandboxError> {
        if request.policy.workspace() != self.workspace {
            return Err(SandboxError::InvalidPath {
                kind: "workspace",
                path: request.policy.workspace().to_owned(),
                reason: format!("backend was probed for `{}`", self.workspace.display()),
            });
        }
        if request.policy.network() == NetworkAccess::Denied
            && let Some(detail) = &self.network_probe_error
        {
            return Err(SandboxError::ProbeFailed {
                capability: "network namespace",
                detail: detail.clone(),
            });
        }
        self.capabilities.supports(&request.policy)?;
        compile_seccomp(request.policy.network())?;

        request.cwd = canonical_directory(&request.cwd, "working directory")?;
        let (mut writable_roots, protected_paths) =
            compile_filesystem_policy(&request, &self.current_exe)?;
        sort_paths(&mut writable_roots);

        let arguments = bwrap_arguments(
            &request,
            &self.current_exe,
            &writable_roots,
            &protected_paths,
        );
        Ok(PreparedCommand::from_backend(
            request,
            self.capabilities
                .executable
                .as_ref()
                .expect("the bubblewrap backend always records its launcher")
                .as_os_str()
                .to_owned(),
            arguments,
            &self.capabilities,
            writable_roots,
            protected_paths,
        ))
    }
}

fn validated_helper(path: &Path) -> Result<PathBuf, SandboxError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| SandboxError::InvalidPath {
            kind: "sandbox helper",
            path: path.to_owned(),
            reason: error.to_string(),
        })?;
    let metadata = fs::metadata(&canonical)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(SandboxError::InvalidPath {
            kind: "sandbox helper",
            path: canonical,
            reason: "expected an executable regular file".to_owned(),
        });
    }
    Ok(canonical)
}

impl SandboxBackend for LinuxBubblewrapSandbox {
    fn capabilities(&self) -> &SandboxCapabilities {
        &self.capabilities
    }

    fn prepare(&self, request: PrepareRequest) -> Result<PreparedCommand, SandboxError> {
        self.prepare_inner(request)
    }
}

fn compile_filesystem_policy(
    request: &PrepareRequest,
    current_exe: &Path,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), SandboxError> {
    let policy = &request.policy;
    let mut writable = BTreeSet::new();
    if policy.mode == SandboxMode::WorkspaceWrite {
        writable.insert(policy.workspace.clone());
    }
    if policy.mode == SandboxMode::WorkspaceWrite {
        for root in &policy.writable_roots {
            reject_filesystem_root(root, "writable root")?;
            writable.insert(canonical_directory(root, "writable root")?);
        }
    }

    let git = git_paths(&policy.workspace)?;
    let mut protected = BTreeSet::new();
    for name in [".zuno", ".agents"] {
        insert_existing(&mut protected, policy.workspace.join(name))?;
    }
    if policy.git_metadata_writable && policy.mode == SandboxMode::WorkspaceWrite {
        if git.marker.is_file() {
            insert_existing(&mut protected, git.marker.clone())?;
        }
        for directory in git.directories {
            reject_filesystem_root(&directory, "Git metadata root")?;
            writable.insert(directory);
        }
    } else {
        insert_existing(&mut protected, git.marker)?;
        for directory in git.directories {
            insert_existing(&mut protected, directory)?;
        }
    }
    for path in &policy.protected_paths {
        insert_existing(&mut protected, path.clone())?;
    }
    insert_existing(&mut protected, current_exe.to_owned())?;

    let mut writable = writable.into_iter().collect::<Vec<_>>();
    let mut protected = protected.into_iter().collect::<Vec<_>>();
    sort_paths(&mut writable);
    sort_paths(&mut protected);
    Ok((writable, protected))
}

#[derive(Debug)]
struct GitPaths {
    marker: PathBuf,
    directories: Vec<PathBuf>,
}

fn git_paths(workspace: &Path) -> Result<GitPaths, SandboxError> {
    let marker = workspace.join(".git");
    if !marker.exists() {
        return Ok(GitPaths {
            marker,
            directories: Vec::new(),
        });
    }
    if marker.is_dir() {
        return Ok(GitPaths {
            marker: marker.clone(),
            directories: vec![marker.canonicalize()?],
        });
    }

    let contents = fs::read_to_string(&marker).map_err(|error| SandboxError::InvalidPath {
        kind: "Git metadata pointer",
        path: marker.clone(),
        reason: error.to_string(),
    })?;
    let value = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| SandboxError::InvalidPath {
            kind: "Git metadata pointer",
            path: marker.clone(),
            reason: "missing `gitdir:` target".to_owned(),
        })?;
    let gitdir = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        workspace.join(value)
    }
    .canonicalize()
    .map_err(|error| SandboxError::InvalidPath {
        kind: "Git metadata directory",
        path: PathBuf::from(value),
        reason: error.to_string(),
    })?;
    let mut directories = vec![gitdir.clone()];
    let common_file = gitdir.join("commondir");
    if common_file.is_file() {
        let common =
            fs::read_to_string(&common_file).map_err(|error| SandboxError::InvalidPath {
                kind: "Git common directory pointer",
                path: common_file.clone(),
                reason: error.to_string(),
            })?;
        let common = common.trim();
        if !common.is_empty() {
            let path = if Path::new(common).is_absolute() {
                PathBuf::from(common)
            } else {
                gitdir.join(common)
            }
            .canonicalize()
            .map_err(|error| SandboxError::InvalidPath {
                kind: "Git common directory",
                path: PathBuf::from(common),
                reason: error.to_string(),
            })?;
            directories.push(path);
        }
    }
    directories.sort();
    directories.dedup();
    Ok(GitPaths {
        marker,
        directories,
    })
}

fn insert_existing(paths: &mut BTreeSet<PathBuf>, path: PathBuf) -> Result<(), SandboxError> {
    if path.exists() {
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(SandboxError::InvalidPath {
                kind: "protected path",
                path,
                reason: "symbolic links cannot be protected safely beneath a writable root"
                    .to_owned(),
            });
        }
        paths.insert(path.canonicalize()?);
    }
    Ok(())
}

fn reject_filesystem_root(path: &Path, kind: &'static str) -> Result<(), SandboxError> {
    if path.parent().is_none() {
        return Err(SandboxError::InvalidPath {
            kind,
            path: path.to_owned(),
            reason: "the filesystem root may not be writable".to_owned(),
        });
    }
    Ok(())
}

fn sort_paths(paths: &mut Vec<PathBuf>) {
    paths.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    paths.dedup();
}

fn bwrap_arguments(
    request: &PrepareRequest,
    current_exe: &Path,
    writable_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Vec<OsString> {
    let mut args = Vec::new();
    extend(
        &mut args,
        [
            "--die-with-parent",
            "--new-session",
            "--unshare-user",
            "--uid",
            "0",
            "--gid",
            "0",
            "--unshare-pid",
            "--unshare-uts",
            "--unshare-ipc",
            "--disable-userns",
            "--assert-userns-disabled",
            "--hostname",
            "zuno-sandbox",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/var/tmp",
        ],
    );
    if request.policy.network == NetworkAccess::Denied {
        args.push("--unshare-net".into());
    }
    extend(&mut args, ["--cap-drop", "ALL"]);

    // `/tmp` and `/var/tmp` are private writable tmpfs mounts. A workspace below
    // either path would otherwise resolve into that tmpfs and bypass its host
    // filesystem mode. Rebind the real workspace read-only before layering any
    // policy-approved writable roots.
    create_private_tmp_parents(&mut args, request.policy.workspace());
    args.push("--ro-bind".into());
    args.push(request.policy.workspace().as_os_str().to_owned());
    args.push(request.policy.workspace().as_os_str().to_owned());

    for root in writable_roots {
        create_private_tmp_parents(&mut args, root);
        args.push("--bind".into());
        args.push(root.as_os_str().to_owned());
        args.push(root.as_os_str().to_owned());
    }
    for path in protected_paths {
        args.push("--ro-bind".into());
        args.push(path.as_os_str().to_owned());
        args.push(path.as_os_str().to_owned());
    }
    extend(
        &mut args,
        [
            "--setenv", "TMPDIR", "/tmp", "--setenv", "TMP", "/tmp", "--setenv", "TEMP", "/tmp",
            "--chdir",
        ],
    );
    args.push(request.cwd.as_os_str().to_owned());
    args.push("--".into());
    args.push(current_exe.as_os_str().to_owned());
    args.push(HELPER_MARKER.into());
    args.push(match request.policy.network {
        NetworkAccess::Denied => "deny".into(),
        NetworkAccess::Allowed => "allow".into(),
    });
    args.push("--".into());
    args.push(request.program.clone());
    args.extend(request.arguments.iter().cloned());
    args
}

fn create_private_tmp_parents(args: &mut Vec<OsString>, path: &Path) {
    let base = if path.starts_with("/tmp") {
        Path::new("/tmp")
    } else if path.starts_with("/var/tmp") {
        Path::new("/var/tmp")
    } else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let mut current = PathBuf::from(base);
    let Ok(relative) = parent.strip_prefix(base) else {
        return;
    };
    for component in relative.components() {
        if let Component::Normal(component) = component {
            current.push(component);
            args.push("--dir".into());
            args.push(current.as_os_str().to_owned());
        }
    }
}

fn extend<const N: usize>(target: &mut Vec<OsString>, values: [&str; N]) {
    target.extend(values.into_iter().map(OsString::from));
}

fn trusted_executable(
    candidates: &[&str],
    workspace: &Path,
    require_root_owner: bool,
) -> Result<PathBuf, SandboxError> {
    let mut rejection = None;
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(_) => continue,
        };
        let metadata = match fs::metadata(&canonical) {
            Ok(metadata) => metadata,
            Err(error) => {
                rejection = Some(SandboxError::UntrustedBubblewrap {
                    path: canonical,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let reason = if canonical.starts_with(workspace) {
            Some("resolved inside the writable workspace".to_owned())
        } else if !metadata.is_file() {
            Some("not a regular file".to_owned())
        } else if metadata.permissions().mode() & 0o111 == 0 {
            Some("not executable".to_owned())
        } else if require_root_owner && metadata.uid() != 0 {
            Some(format!("owned by uid {}, expected root", metadata.uid()))
        } else {
            None
        };
        if let Some(reason) = reason {
            rejection = Some(SandboxError::UntrustedBubblewrap {
                path: canonical,
                reason,
            });
            continue;
        }
        return Ok(canonical);
    }
    Err(rejection.unwrap_or(SandboxError::BubblewrapNotFound))
}

fn require_bwrap_options(bwrap: &Path) -> Result<(), SandboxError> {
    let output = Command::new(bwrap).arg("--help").output()?;
    if !output.status.success() {
        return Err(SandboxError::ProbeFailed {
            capability: "bubblewrap help",
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let help = String::from_utf8_lossy(&output.stdout);
    for option in REQUIRED_BWRAP_OPTIONS {
        if !help.contains(option) {
            return Err(SandboxError::MissingBubblewrapCapability(
                (*option).to_owned(),
            ));
        }
    }
    Ok(())
}

fn run_probe(bwrap: &Path, true_executable: &Path, network: bool) -> Result<(), String> {
    let mut command = Command::new(bwrap);
    command.args([
        OsStr::new("--die-with-parent"),
        OsStr::new("--new-session"),
        OsStr::new("--unshare-user"),
        OsStr::new("--uid"),
        OsStr::new("0"),
        OsStr::new("--gid"),
        OsStr::new("0"),
        OsStr::new("--unshare-pid"),
        OsStr::new("--unshare-uts"),
        OsStr::new("--unshare-ipc"),
        OsStr::new("--disable-userns"),
        OsStr::new("--assert-userns-disabled"),
        OsStr::new("--ro-bind"),
        OsStr::new("/"),
        OsStr::new("/"),
        OsStr::new("--dev"),
        OsStr::new("/dev"),
        OsStr::new("--proc"),
        OsStr::new("/proc"),
        OsStr::new("--cap-drop"),
        OsStr::new("ALL"),
    ]);
    if network {
        command.arg("--unshare-net");
    }
    command
        .arg("--")
        .arg(true_executable)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| error.to_string())?;
                if output.status.success() {
                    return Ok(());
                }
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                return Err(if stderr.is_empty() {
                    format!("bubblewrap exited with {}", output.status)
                } else {
                    stderr
                });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "bubblewrap probe exceeded {:.1}s",
                    PROBE_TIMEOUT.as_secs_f64()
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn reject_wsl1() -> Result<(), SandboxError> {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let lowered = release.to_ascii_lowercase();
    if lowered.contains("microsoft") && !lowered.contains("wsl2") {
        return Err(SandboxError::Wsl1Unsupported);
    }
    Ok(())
}

fn target_arch() -> Result<TargetArch, SandboxError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(TargetArch::x86_64),
        "aarch64" => Ok(TargetArch::aarch64),
        other => Err(SandboxError::UnsupportedArchitecture(other.to_owned())),
    }
}

fn compile_seccomp(network: NetworkAccess) -> Result<BpfProgram, SandboxError> {
    let mut rules = BTreeMap::new();
    for syscall in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        rules.insert(syscall, Vec::new());
    }
    if network == NetworkAccess::Denied {
        for syscall in [
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_bind,
            libc::SYS_connect,
            libc::SYS_getpeername,
            libc::SYS_getsockname,
            libc::SYS_getsockopt,
            libc::SYS_listen,
            libc::SYS_recvmmsg,
            libc::SYS_sendmmsg,
            libc::SYS_sendto,
            libc::SYS_setsockopt,
            libc::SYS_shutdown,
        ] {
            rules.insert(syscall, Vec::new());
        }
        let non_unix = SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                libc::AF_UNIX as u64,
            )
            .map_err(|error| SandboxError::Seccomp(error.to_string()))?,
        ])
        .map_err(|error| SandboxError::Seccomp(error.to_string()))?;
        rules.insert(libc::SYS_socket, vec![non_unix.clone()]);
        rules.insert(libc::SYS_socketpair, vec![non_unix]);
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        target_arch()?,
    )
    .map_err(|error| SandboxError::Seccomp(error.to_string()))?;
    filter
        .try_into()
        .map_err(|error: seccompiler::BackendError| SandboxError::Seccomp(error.to_string()))
}

pub(crate) fn run_helper_from_args() -> Option<ExitCode> {
    let mut args = std::env::args_os();
    let _executable = args.next();
    if args.next().as_deref() != Some(OsStr::new(HELPER_MARKER)) {
        return None;
    }
    let result = helper_main(args.collect());
    Some(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zuno sandbox helper: {error}");
            ExitCode::FAILURE
        }
    })
}

fn helper_main(arguments: Vec<OsString>) -> Result<(), SandboxError> {
    let mut arguments = arguments.into_iter();
    let network = match arguments.next().as_deref().and_then(OsStr::to_str) {
        Some("deny") => NetworkAccess::Denied,
        Some("allow") => NetworkAccess::Allowed,
        _ => {
            return Err(SandboxError::Helper(
                "expected network mode `deny` or `allow`".to_owned(),
            ));
        }
    };
    if arguments.next().as_deref() != Some(OsStr::new("--")) {
        return Err(SandboxError::Helper(
            "expected `--` before the command".to_owned(),
        ));
    }
    let program = arguments
        .next()
        .ok_or_else(|| SandboxError::Helper("missing command program".to_owned()))?;
    let command_arguments = arguments.collect::<Vec<_>>();

    verify_capabilities_dropped()?;
    rustix::thread::set_no_new_privs(true)
        .map_err(|error| SandboxError::Helper(format!("PR_SET_NO_NEW_PRIVS failed: {error}")))?;
    let filter = compile_seccomp(network)?;
    seccompiler::apply_filter(&filter)
        .map_err(|error| SandboxError::Helper(format!("installing seccomp failed: {error}")))?;

    let error = Command::new(&program).args(command_arguments).exec();
    Err(SandboxError::Helper(format!(
        "executing `{}` failed: {error}",
        program.to_string_lossy()
    )))
}

fn verify_capabilities_dropped() -> Result<(), SandboxError> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|error| SandboxError::Helper(format!("reading capabilities failed: {error}")))?;
    for field in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .map(str::trim)
            .ok_or_else(|| {
                SandboxError::Helper(format!("missing `{field}` in /proc/self/status"))
            })?;
        let value = u64::from_str_radix(value, 16).map_err(|error| {
            SandboxError::Helper(format!("invalid `{field}` value `{value}`: {error}"))
        })?;
        if value != 0 {
            return Err(SandboxError::Helper(format!(
                "`{field}` is not zero after bubblewrap capability drop"
            )));
        }
    }
    Ok(())
}

fn canonical_directory(path: &Path, kind: &'static str) -> Result<PathBuf, SandboxError> {
    let canonical = path
        .canonicalize()
        .map_err(|error| SandboxError::InvalidPath {
            kind,
            path: path.to_owned(),
            reason: error.to_string(),
        })?;
    if !canonical.is_dir() {
        return Err(SandboxError::InvalidPath {
            kind,
            path: canonical,
            reason: "expected a directory".to_owned(),
        });
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxPolicy;
    use std::collections::BTreeMap;

    fn backend(workspace: &Path, network_isolation: bool) -> LinuxBubblewrapSandbox {
        LinuxBubblewrapSandbox {
            workspace: workspace.to_owned(),
            current_exe: PathBuf::from("/usr/bin/zuno"),
            capabilities: SandboxCapabilities {
                backend: BACKEND_NAME.to_owned(),
                executable: Some(PathBuf::from("/usr/bin/bwrap")),
                read_only: true,
                workspace_write: true,
                danger_full_access: false,
                network_isolation,
            },
            network_probe_error: (!network_isolation).then(|| "network probe failed".to_owned()),
        }
    }

    #[test]
    fn backend_records_network_isolation_only_after_a_successful_probe() {
        let workspace = tempfile::tempdir().expect("workspace");
        let unavailable = backend(workspace.path(), false);
        let available = backend(workspace.path(), true);

        assert!(!unavailable.capabilities().network_isolation);
        assert!(available.capabilities().network_isolation);
    }

    #[test]
    fn preparation_wraps_the_command_in_bwrap_instead_of_returning_raw_argv() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = backend(workspace.path(), true);
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-lc"), OsString::from("printf ok")],
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::new(),
            policy: SandboxPolicy::new(
                workspace.path(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Allowed,
            )
            .expect("policy"),
        };

        let prepared = backend.prepare(request).expect("prepared command");
        let parts = prepared.into_parts();

        assert_eq!(parts.program, OsString::from("/usr/bin/bwrap"));
        assert!(parts.arguments.windows(3).any(|window| {
            window
                == [
                    OsString::from("--ro-bind"),
                    OsString::from("/"),
                    OsString::from("/"),
                ]
        }));
        assert!(parts.arguments.contains(&OsString::from("--cap-drop")));
        assert!(parts.arguments.contains(&OsString::from(HELPER_MARKER)));
    }

    #[test]
    fn workspace_write_reapplies_zuno_agents_and_git_as_read_only() {
        let workspace = tempfile::tempdir().expect("workspace");
        for path in [".git", ".zuno", ".agents"] {
            fs::create_dir(workspace.path().join(path)).expect("protected directory");
        }
        let executable = workspace.path().join("zuno");
        fs::write(&executable, b"binary").expect("helper");
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: Vec::new(),
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::new(),
            policy: SandboxPolicy::new(
                workspace.path(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Allowed,
            )
            .expect("policy"),
        };

        let (writable, protected) =
            compile_filesystem_policy(&request, &executable).expect("compiled policy");

        assert_eq!(
            writable,
            [workspace.path().canonicalize().expect("workspace")]
        );
        for path in [".git", ".zuno", ".agents"] {
            assert!(
                protected.contains(
                    &workspace
                        .path()
                        .join(path)
                        .canonicalize()
                        .expect("protected path")
                ),
                "{path} was not protected: {protected:?}"
            );
        }
        assert!(protected.contains(&executable.canonicalize().expect("helper")));
    }

    #[test]
    fn git_write_grant_opens_only_git_metadata_and_keeps_runtime_state_protected() {
        let workspace = tempfile::tempdir().expect("workspace");
        for path in [".git", ".zuno", ".agents"] {
            fs::create_dir(workspace.path().join(path)).expect("protected directory");
        }
        let executable = workspace.path().join("zuno");
        fs::write(&executable, b"binary").expect("helper");
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: Vec::new(),
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::new(),
            policy: SandboxPolicy::new(
                workspace.path(),
                SandboxMode::WorkspaceWrite,
                NetworkAccess::Allowed,
            )
            .expect("policy")
            .with_git_metadata_writable(true),
        };

        let (writable, protected) =
            compile_filesystem_policy(&request, &executable).expect("compiled policy");
        let git = workspace.path().join(".git").canonicalize().expect("git");

        assert!(writable.contains(&git));
        assert!(!protected.contains(&git));
        for path in [".zuno", ".agents"] {
            assert!(
                protected.contains(
                    &workspace
                        .path()
                        .join(path)
                        .canonicalize()
                        .expect("protected path")
                ),
                "{path} was not protected"
            );
        }
    }

    #[test]
    fn denied_network_compiles_namespace_and_helper_policy() {
        let workspace = tempfile::tempdir().expect("workspace");
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: Vec::new(),
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::new(),
            policy: SandboxPolicy::new(
                workspace.path(),
                SandboxMode::ReadOnly,
                NetworkAccess::Denied,
            )
            .expect("policy"),
        };
        let args = bwrap_arguments(&request, Path::new("/usr/bin/zuno"), &[], &[]);

        assert!(args.contains(&OsString::from("--unshare-net")));
        assert!(
            args.windows(2)
                .any(|window| window == [OsString::from(HELPER_MARKER), OsString::from("deny")])
        );
        compile_seccomp(NetworkAccess::Denied).expect("seccomp policy");
    }

    #[test]
    fn network_probe_error_is_typed_and_never_falls_back_to_host_network() {
        let workspace = tempfile::tempdir().expect("workspace");
        let backend = backend(workspace.path(), false);
        let request = PrepareRequest {
            program: OsString::from("/bin/sh"),
            arguments: Vec::new(),
            cwd: workspace.path().to_owned(),
            environment: BTreeMap::new(),
            policy: SandboxPolicy::new(
                workspace.path(),
                SandboxMode::ReadOnly,
                NetworkAccess::Denied,
            )
            .expect("policy"),
        };

        let error = backend
            .prepare(request)
            .expect_err("network denial must fail closed");

        assert!(matches!(
            error,
            SandboxError::ProbeFailed {
                capability: "network namespace",
                ..
            }
        ));
    }
}
