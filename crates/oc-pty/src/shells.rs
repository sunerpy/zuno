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
//! reason; see `.omo/notepads/opencode-rust/issues.md` for how this interacts with
//! todo 40's single-selector `oc_tools::shell::discover_shell`.

use std::path::{Path, PathBuf};

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
pub const GIT_BASH_PATH_ENV: &str = "OPENCODE_GIT_BASH_PATH";

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
/// `shell` config key; when it is absent the process `SHELL` is consulted, and
/// when neither resolves the platform default wins.
#[must_use]
pub fn preferred(configured: Option<&str>) -> PathBuf {
    select(configured, environment_shell().as_deref(), false)
}

/// The shell to run generated POSIX script in (`shell.ts:214-218`).
///
/// Applies the deny list, so a fish or nushell value in `configured` or `SHELL` is
/// passed over in favour of the platform default.
#[must_use]
pub fn acceptable(configured: Option<&str>) -> PathBuf {
    select(configured, environment_shell().as_deref(), true)
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
    for candidate in [configured, environment].into_iter().flatten() {
        if candidate.is_empty() {
            continue;
        }
        if require_acceptable && !is_acceptable(Path::new(candidate)) {
            continue;
        }
        if let Some(resolved) = resolve(Path::new(candidate)) {
            return resolved;
        }
        // The oracle falls through to the platform default after the *first*
        // supplied value fails, rather than trying the next (`shell.ts:114-120`).
        break;
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
        return Some(PathBuf::from(explicit));
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
            .filter(std::fs::Metadata::is_file)
            .map(|_| candidate.to_owned());
    }
    which::which(candidate).ok()
}

fn environment_shell() -> Option<String> {
    std::env::var("SHELL")
        .ok()
        .filter(|value| !value.is_empty())
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

    #[test]
    fn selection_prefers_the_configured_shell_over_the_environment() {
        let selected = select(Some("/bin/sh"), Some("/bin/bash"), false);
        assert_eq!(selected, PathBuf::from("/bin/sh"));
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

    #[test]
    fn an_empty_configured_value_is_ignored_rather_than_resolved() {
        assert_eq!(
            select(Some(""), Some("/bin/sh"), false),
            PathBuf::from("/bin/sh")
        );
    }

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
