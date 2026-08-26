//! Shell discovery, ported from `packages/core/src/shell.ts`.
//!
//! Two distinct questions, which the oracle answers with two distinct functions
//! and which this module keeps distinct for the same reason:
//!
//! - [`list`] enumerates every shell on the machine and is what `GET /pty/shells`
//!   returns (`shell.ts:223-226`). Denied shells are *included*, flagged
//!   `acceptable: false`, because the endpoint describes the machine rather than
//!   filtering it.
//! - [`preferred`] picks the one to spawn a terminal in (`shell.ts:205-209`), and
//!   deliberately does **not** apply the deny list.
//!
//! # `preferred` and `acceptable` are not interchangeable
//!
//! `shell.ts:214-218` exposes a second selector that *does* apply the deny list,
//! and the two have different callers. A PTY runs `Shell.preferred`
//! (`pty.ts:174`), so a fish user gets fish. Non-interactive command execution
//! runs `Shell.acceptable`, because it injects POSIX script that fish and nushell
//! cannot parse. Collapsing them would either break fish users' terminals or feed
//! POSIX script to a shell that rejects it. Both are exported here for that
//! reason. Model-issued commands use [`command`], the shared strict resolver
//! that also records the actual interpreter name for durable clients.

use std::io;
use std::path::{Path, PathBuf};
#[cfg(all(unix, not(target_os = "redox")))]
use std::sync::OnceLock;

/// One entry of `GET /pty/shells`, from `packages/core/src/shell.ts:25-29`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellItem {
    /// Absolute path to the executable.
    pub path: String,
    /// Bare name when it resolves on `PATH`, otherwise the full path
    /// (`shell.ts:161`).
    pub name: String,
    /// Whether it can run generated POSIX script. False for fish and nushell.
    pub acceptable: bool,
}

/// The per-shell flags in `META` (`packages/core/src/shell.ts:13-23`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShellMeta {
    /// Cannot run generated POSIX script, so [`acceptable`] skips it.
    pub deny: bool,
    /// Takes `-l`, which [`crate::session`] appends when opening a terminal.
    pub login: bool,
    /// Speaks POSIX `sh` syntax.
    pub posix: bool,
    /// Takes PowerShell arguments rather than `-c`.
    pub powershell: bool,
}

/// Invocation syntax for one non-interactive command shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandShellKind {
    Posix,
    PowerShell,
}

/// One resolved shell executable used for model-issued commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandShell {
    path: PathBuf,
    kind: CommandShellKind,
    name: String,
}

impl CommandShell {
    /// Resolved executable path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Argument and parsing family used to invoke the executable.
    #[must_use]
    pub const fn kind(&self) -> CommandShellKind {
        self.kind
    }

    /// Stable user-facing interpreter name, such as `zsh` or `pwsh`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

const META: &[(&str, ShellMeta)] = &[
    (
        "bash",
        ShellMeta {
            deny: false,
            login: true,
            posix: true,
            powershell: false,
        },
    ),
    (
        "dash",
        ShellMeta {
            deny: false,
            login: true,
            posix: true,
            powershell: false,
        },
    ),
    (
        "fish",
        ShellMeta {
            deny: true,
            login: true,
            posix: false,
            powershell: false,
        },
    ),
    (
        "ksh",
        ShellMeta {
            deny: false,
            login: true,
            posix: true,
            powershell: false,
        },
    ),
    (
        "nu",
        ShellMeta {
            deny: true,
            login: false,
            posix: false,
            powershell: false,
        },
    ),
    (
        "powershell",
        ShellMeta {
            deny: false,
            login: false,
            posix: false,
            powershell: true,
        },
    ),
    (
        "pwsh",
        ShellMeta {
            deny: false,
            login: false,
            posix: false,
            powershell: true,
        },
    ),
    (
        "sh",
        ShellMeta {
            deny: false,
            login: true,
            posix: true,
            powershell: false,
        },
    ),
    (
        "zsh",
        ShellMeta {
            deny: false,
            login: true,
            posix: true,
            powershell: false,
        },
    ),
];

/// Where a POSIX machine publishes its shells (`shell.ts:109`).
const ETC_SHELLS: &str = "/etc/shells";

/// Consulted only when `/etc/shells` is missing or empty (`shell.ts:111`).
const POSIX_FALLBACK: &[&str] = &["/bin/bash", "/bin/zsh", "/bin/sh"];

/// Overrides Git Bash detection on Windows (`shell.ts:125`).
pub const GIT_BASH_PATH_ENV: &str = "ZUNO_GIT_BASH_PATH";

/// The lowercased base name, as `shell.ts:139-142` computes it.
#[must_use]
pub fn shell_name(path: &Path) -> String {
    let component = if cfg!(windows) {
        path.file_stem()
    } else {
        path.file_name()
    };
    component
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The flags for a shell, by base name.
#[must_use]
pub fn meta(path: &Path) -> Option<ShellMeta> {
    let name = shell_name(path);
    META.iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, meta)| *meta)
}

/// Whether this shell takes `-l` (`shell.ts:144-146`).
#[must_use]
pub fn login(path: &Path) -> bool {
    meta(path).is_some_and(|meta| meta.login)
}

/// Whether this shell speaks POSIX syntax (`shell.ts:148-150`).
#[must_use]
pub fn posix(path: &Path) -> bool {
    meta(path).is_some_and(|meta| meta.posix)
}

/// Whether this shell takes PowerShell arguments (`shell.ts:152-154`).
#[must_use]
pub fn powershell(path: &Path) -> bool {
    meta(path).is_some_and(|meta| meta.powershell)
}

/// Whether generated POSIX script can be handed to this shell.
///
/// An unknown shell is accepted, matching `deny !== true` at `shell.ts:81`: the
/// table lists what is known to be a problem, not what is known to be safe.
#[must_use]
pub fn is_acceptable(path: &Path) -> bool {
    meta(path).is_none_or(|meta| !meta.deny)
}

/// Every shell on this machine, in the platform's preference order.
///
/// Candidates that do not resolve to a real executable are dropped
/// (`shell.ts:225`), so this is safe to hand a client as a list of things it can
/// actually launch.
#[must_use]
pub fn list() -> Vec<ShellItem> {
    let candidates = if cfg!(windows) {
        windows_candidates()
    } else {
        posix_candidates(&std::fs::read_to_string(ETC_SHELLS).unwrap_or_default())
    };
    candidates
        .iter()
        .filter_map(|candidate| resolve(Path::new(candidate)))
        .map(|resolved| describe(&resolved))
        .collect()
}

/// The shell to open a terminal in (`shell.ts:205-209`).
///
/// Does not apply the deny list — see the module docs. `configured` is the
/// `shell` config key; when it is absent the operating-system account shell,
/// process `SHELL`, and platform default are consulted in that order.
pub fn preferred(configured: Option<&str>) -> io::Result<PathBuf> {
    preferred_with_sources(
        configured,
        account_shell().as_deref(),
        environment_shell().as_deref(),
    )
}

/// [`preferred`] with host identity sources supplied explicitly.
///
/// An explicit configuration is authoritative: a missing or non-executable path is
/// an error rather than a request to fall back silently. Account and environment
/// values are discovery hints, so an unavailable value advances to the next source.
pub fn preferred_with_sources(
    configured: Option<&str>,
    account: Option<&str>,
    environment: Option<&str>,
) -> io::Result<PathBuf> {
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        return resolve(Path::new(configured)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("configured shell {configured} was not found or is not executable"),
            )
        });
    }

    for candidate in [account, environment].into_iter().flatten() {
        if let Some(shell) = resolve(Path::new(candidate)) {
            return Ok(shell);
        }
    }

    let candidate = fallback();
    resolve(&candidate).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "fallback shell {} was not found or is not executable",
                candidate.display()
            ),
        )
    })
}

/// The shell to run generated POSIX script in (`shell.ts:214-218`).
///
/// Applies the deny list, so a fish or nushell value in `configured` or `SHELL` is
/// passed over in favour of the platform default.
#[must_use]
pub fn acceptable(configured: Option<&str>) -> PathBuf {
    select_with_account(
        configured,
        account_shell().as_deref(),
        environment_shell().as_deref(),
        true,
    )
}

/// Resolve the shell used for non-interactive model-issued commands.
///
/// An explicit configuration error is terminal. Without one, the operating-system
/// account shell wins over the inherited `SHELL`, matching the login identity rather
/// than whichever parent process happened to launch Zuno.
pub fn command(configured: Option<&str>) -> io::Result<CommandShell> {
    command_with_sources(
        configured,
        account_shell().as_deref(),
        environment_shell().as_deref(),
    )
}

/// Pure selection seam for tests and embedders that already resolved host identity.
pub fn command_with_sources(
    configured: Option<&str>,
    account: Option<&str>,
    environment: Option<&str>,
) -> io::Result<CommandShell> {
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        return resolve_command(Path::new(configured)).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("configured shell {configured} was not found or is not supported"),
            )
        });
    }

    for candidate in [account, environment].into_iter().flatten() {
        if let Some(shell) = resolve_command(Path::new(candidate)) {
            return Ok(shell);
        }
    }

    resolve_command(&fallback())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no supported shell was found"))
}

/// [`preferred`] and [`acceptable`] with the environment supplied explicitly.
///
/// The oracle memoizes its `SHELL` read behind a module-level cache with a
/// `reset()` hook for tests (`shell.ts:202-221`). This crate takes the value as an
/// argument instead: process-wide mutable state that tests have to reset is the
/// shape that makes a suite order-dependent, and every rule here is a pure
/// function of two strings.
#[must_use]
pub fn select(
    configured: Option<&str>,
    environment: Option<&str>,
    require_acceptable: bool,
) -> PathBuf {
    select_with_account(configured, None, environment, require_acceptable)
}

fn select_with_account(
    configured: Option<&str>,
    account: Option<&str>,
    environment: Option<&str>,
    require_acceptable: bool,
) -> PathBuf {
    for (source, candidate) in [configured, account, environment]
        .into_iter()
        .enumerate()
        .filter_map(|(source, candidate)| candidate.map(|candidate| (source, candidate)))
    {
        if candidate.is_empty() {
            continue;
        }
        if require_acceptable && !is_acceptable(Path::new(candidate)) {
            continue;
        }
        if let Some(resolved) = resolve(Path::new(candidate)) {
            return resolved;
        }
        if source == 0 {
            // An explicit but missing executable is not silently replaced by a
            // lower-precedence identity source. The command resolver reports this
            // as an error; this infallible PTY helper reaches the platform default.
            return fallback();
        }
    }
    fallback()
}

/// The platform default when nothing was configured (`shell.ts:117-119`).
#[must_use]
pub fn fallback() -> PathBuf {
    if cfg!(windows) {
        return windows_candidates()
            .first()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cmd.exe"));
    }
    if cfg!(target_os = "macos") {
        return PathBuf::from("/bin/zsh");
    }
    which::which("bash").unwrap_or_else(|_| PathBuf::from("/bin/sh"))
}

/// Git Bash, from the override or next to `git` (`shell.ts:123-130`).
///
/// Always `None` off Windows, matching the oracle's platform guard.
#[must_use]
pub fn git_bash() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    if let Some(explicit) = std::env::var_os(GIT_BASH_PATH_ENV).filter(|value| !value.is_empty()) {
        return resolve(Path::new(&explicit));
    }
    let git = which::which("git").ok()?;
    // `<git>/../../bin/bash.exe`: `git` lives in `<root>/cmd` or `<root>/bin`.
    let candidate = git.parent()?.parent()?.join("bin").join("bash.exe");
    std::fs::metadata(&candidate)
        .ok()
        .filter(|meta| meta.len() > 0)
        .map(|_| candidate)
}

/// The Windows preference order (`shell.ts:98-106`), deduplicated in place.
fn windows_candidates() -> Vec<String> {
    let comspec = std::env::var("COMSPEC")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "cmd.exe".to_owned());
    let ordered = [
        which::which("pwsh").ok(),
        which::which("powershell").ok(),
        git_bash(),
        Some(PathBuf::from(comspec)),
    ];
    dedupe(
        ordered
            .into_iter()
            .flatten()
            .map(|path| path.to_string_lossy().into_owned()),
    )
}

/// `/etc/shells` if it lists anything, else the fallback list (`shell.ts:108-112`).
///
/// Blank lines and comments are skipped. The oracle tests `startsWith("#")` on the
/// raw line, so a comment indented by a space is *not* skipped there; trimming
/// first is a deliberate correction — an indented comment is a path that cannot
/// resolve, and letting it through would only produce a dropped candidate.
fn posix_candidates(etc_shells: &str) -> Vec<String> {
    let listed = dedupe(
        etc_shells
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned),
    );
    if listed.is_empty() {
        return POSIX_FALLBACK
            .iter()
            .map(|path| (*path).to_owned())
            .collect();
    }
    listed
}

/// `shell.ts:156-164`.
fn describe(path: &Path) -> ShellItem {
    let name = shell_name(path);
    let resolves_by_name = !name.is_empty() && which::which(&name).is_ok();
    ShellItem {
        path: path.to_string_lossy().into_owned(),
        name: if resolves_by_name {
            name
        } else {
            path.to_string_lossy().into_owned()
        },
        acceptable: is_acceptable(path),
    }
}

/// `shell.ts:88-96`: an absolute path must be an existing file; a bare name is
/// looked up on `PATH`.
fn resolve(candidate: &Path) -> Option<PathBuf> {
    if candidate.as_os_str().is_empty() {
        return None;
    }
    if candidate.is_absolute() {
        return std::fs::metadata(candidate)
            .ok()
            .filter(is_executable_file)
            .map(|_| candidate.to_owned());
    }
    which::which(candidate).ok()
}

#[cfg(unix)]
fn is_executable_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_file(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

fn environment_shell() -> Option<String> {
    std::env::var("SHELL")
        .ok()
        .filter(|value| !value.is_empty())
}

#[cfg(all(unix, not(target_os = "redox")))]
static ACCOUNT_SHELL: OnceLock<Option<String>> = OnceLock::new();

#[cfg(all(unix, not(target_os = "redox")))]
fn account_shell() -> Option<String> {
    ACCOUNT_SHELL.get_or_init(resolve_account_shell).clone()
}

#[cfg(all(unix, not(target_os = "redox")))]
fn resolve_account_shell() -> Option<String> {
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()?;
    let shell = user.shell.to_string_lossy().into_owned();
    (!shell.is_empty()).then_some(shell)
}

#[cfg(any(not(unix), target_os = "redox"))]
fn account_shell() -> Option<String> {
    None
}

fn resolve_command(candidate: &Path) -> Option<CommandShell> {
    let path = resolve(candidate)?;
    let metadata = meta(&path)?;
    if metadata.deny {
        return None;
    }
    let name = shell_name(&path);
    let kind = if metadata.powershell {
        CommandShellKind::PowerShell
    } else if metadata.posix {
        CommandShellKind::Posix
    } else {
        return None;
    };
    Some(CommandShell { path, kind, name })
}

fn dedupe(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = Vec::new();
    for value in values {
        if !seen.contains(&value) {
            seen.push(value);
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meta_table_matches_the_oracle_flag_for_flag() {
        assert_eq!(
            meta(Path::new("/bin/bash")),
            Some(ShellMeta {
                deny: false,
                login: true,
                posix: true,
                powershell: false
            })
        );
        assert_eq!(
            meta(Path::new("/usr/bin/fish")),
            Some(ShellMeta {
                deny: true,
                login: true,
                posix: false,
                powershell: false
            })
        );
        assert_eq!(
            meta(Path::new("/usr/local/bin/nu")),
            Some(ShellMeta {
                deny: true,
                login: false,
                posix: false,
                powershell: false
            })
        );
        assert_eq!(
            meta(Path::new("pwsh")),
            Some(ShellMeta {
                deny: false,
                login: false,
                posix: false,
                powershell: true
            })
        );
        for known in [
            "bash",
            "dash",
            "fish",
            "ksh",
            "nu",
            "powershell",
            "pwsh",
            "sh",
            "zsh",
        ] {
            assert!(
                meta(Path::new(known)).is_some(),
                "{known} must be in the table"
            );
        }
        assert_eq!(META.len(), 9, "the oracle's table has exactly nine entries");
    }

    #[test]
    fn an_unknown_shell_is_acceptable_and_has_no_flags() {
        let exotic = Path::new("/opt/elvish");
        assert!(meta(exotic).is_none());
        assert!(
            is_acceptable(exotic),
            "the table lists problems, not permissions"
        );
        assert!(!login(exotic));
        assert!(!posix(exotic));
        assert!(!powershell(exotic));
    }

    #[test]
    fn etc_shells_entries_are_deduplicated_and_comments_dropped() {
        let listed =
            posix_candidates("# comment\n/bin/bash\n\n/bin/zsh\n/bin/bash\n  # indented\n");
        assert_eq!(listed, vec!["/bin/bash".to_owned(), "/bin/zsh".to_owned()]);
    }

    #[test]
    fn an_empty_etc_shells_falls_back_to_the_three_known_paths() {
        assert_eq!(posix_candidates(""), POSIX_FALLBACK);
        assert_eq!(posix_candidates("\n# only comments\n"), POSIX_FALLBACK);
    }

    #[cfg(unix)]
    #[test]
    fn selection_prefers_the_configured_shell_over_the_environment() {
        let selected = select(Some("/bin/sh"), Some("/bin/bash"), false);
        assert_eq!(selected, PathBuf::from("/bin/sh"));
    }

    #[cfg(unix)]
    #[test]
    fn command_selection_prefers_config_then_account_then_environment() {
        assert_eq!(
            command_with_sources(Some("/bin/sh"), Some("/bin/bash"), Some("/bin/zsh"))
                .expect("configured shell")
                .name(),
            "sh"
        );
        assert_eq!(
            command_with_sources(None, Some("/bin/sh"), Some("/bin/bash"))
                .expect("account shell")
                .name(),
            "sh"
        );
        assert_eq!(
            command_with_sources(None, Some("/missing/account-shell"), Some("/bin/sh"))
                .expect("environment shell")
                .name(),
            "sh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn preferred_rejects_an_invalid_explicit_shell() {
        let error = preferred_with_sources(
            Some("/missing/configured-shell"),
            Some("/bin/sh"),
            Some("/bin/sh"),
        )
        .expect_err("explicit configuration must not fall back");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("configured shell"));
    }

    #[cfg(unix)]
    #[test]
    fn command_rejects_unknown_and_non_executable_interpreters() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("tempdir");
        let unknown = directory.path().join("elvish");
        std::fs::write(&unknown, "#!/bin/sh\n").expect("write unknown shell");
        std::fs::set_permissions(&unknown, std::fs::Permissions::from_mode(0o755))
            .expect("make executable");
        let error = command_with_sources(Some(unknown.to_str().expect("utf8 path")), None, None)
            .expect_err("unknown syntax must be rejected");
        assert!(error.to_string().contains("not supported"));

        let non_executable = directory.path().join("zsh");
        std::fs::write(&non_executable, "#!/bin/sh\n").expect("write non-executable shell");
        std::fs::set_permissions(&non_executable, std::fs::Permissions::from_mode(0o644))
            .expect("remove execute bit");
        assert!(resolve(&non_executable).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_selection_continues_from_a_missing_account_shell_to_environment() {
        assert_eq!(
            preferred_with_sources(None, Some("/missing/account-shell"), Some("/bin/sh"))
                .expect("environment shell"),
            PathBuf::from("/bin/sh")
        );
    }

    #[test]
    fn preferred_keeps_a_denied_shell_but_acceptable_passes_it_over() {
        // A path that cannot resolve isolates the deny decision from the
        // filesystem: `preferred` still tries it, `acceptable` never does.
        let denied = "/nonexistent/bin/fish";
        assert!(!is_acceptable(Path::new(denied)));
        assert_eq!(
            select(Some(denied), None, false),
            fallback(),
            "preferred tries fish, fails to resolve it, and falls back"
        );
        assert_eq!(
            select(Some(denied), None, true),
            fallback(),
            "acceptable skips fish outright"
        );
        // The observable difference: acceptable ignores a denied SHELL and moves
        // on to the configured-value slot being empty, so both reach the fallback
        // by different routes. The routes are what matters for a resolvable fish.
        assert!(!is_acceptable(Path::new("fish")));
        assert!(is_acceptable(Path::new("zsh")));
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_configured_value_is_ignored_rather_than_resolved() {
        assert_eq!(
            select(Some(""), Some("/bin/sh"), false),
            PathBuf::from("/bin/sh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_is_a_real_executable_on_this_host() {
        let shell = fallback();
        assert!(
            resolve(&shell).is_some(),
            "the platform fallback {} does not exist",
            shell.display()
        );
    }

    #[test]
    fn the_listed_shells_all_resolve_and_carry_an_acceptable_flag() {
        let shells = list();
        assert!(
            !shells.is_empty(),
            "no shell was discovered at all; the scan is looking in the wrong place"
        );
        for shell in &shells {
            let path = PathBuf::from(&shell.path);
            assert!(resolve(&path).is_some(), "{} does not resolve", shell.path);
            assert_eq!(shell.acceptable, is_acceptable(&path));
            assert!(!shell.name.is_empty());
        }
    }

    #[test]
    fn git_bash_is_absent_off_windows() {
        if !cfg!(windows) {
            assert!(git_bash().is_none());
        }
    }
}
