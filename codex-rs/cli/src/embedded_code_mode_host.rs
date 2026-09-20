//! The single-file (musl) distribution carries `codex-code-mode-host` inside `zuno`.
//!
//! Models whose catalog entry says `tool_mode = "code_mode_only"` route every tool call
//! through the code-mode host, which upstream ships as a sibling executable. A
//! standalone binary has no siblings, so the host is compiled in and reached through
//! the arg0 trick: `zuno` links itself as `codex-code-mode-host` inside the per-session
//! alias directory and registers that path as the host program. Spawning the alias
//! lands in [`dispatch_if_requested`], which runs the host over stdio.
//!
//! This module only exists on the musl targets that the dependency table in
//! `Cargo.toml` selects; a cargo feature would violate the workspace manifest policy.

use std::path::Path;
use std::path::PathBuf;

use codex_arg0::Arg0DispatchPaths;
use codex_install_context::CODE_MODE_HOST_EXECUTABLE_NAME;
use codex_install_context::InstallContext;

/// Run the embedded code-mode host and exit when this process was started through
/// the `codex-code-mode-host` alias. Returns immediately for a regular `zuno` start.
pub fn dispatch_if_requested() {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    let exe_name = Path::new(&argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if exe_name != CODE_MODE_HOST_EXECUTABLE_NAME {
        return;
    }
    let exit_code = match run_host() {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{CODE_MODE_HOST_EXECUTABLE_NAME}: {err:#}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn run_host() -> anyhow::Result<()> {
    // Mirror the stand-alone host binary: INFO logs on stderr, which the parent
    // forwards into its own tracing output.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(codex_code_mode_host::run_stdio())
}

/// Register an alias of this executable as the code-mode host program.
///
/// Reuses the per-session alias directory that arg0 dispatch already created and
/// keeps locked for the process lifetime, so the alias disappears with it. Returns
/// the alias path, or `None` when no alias directory exists; the host then stays
/// unavailable exactly as it would in a build without this feature.
pub fn install_alias(arg0_paths: &Arg0DispatchPaths) -> Option<PathBuf> {
    let alias_dir = alias_directory(arg0_paths)?;
    let alias = alias_dir.join(CODE_MODE_HOST_EXECUTABLE_NAME);
    let current_exe = arg0_paths
        .codex_self_exe
        .clone()
        .or_else(|| std::env::current_exe().ok())?;
    if !alias.exists()
        && let Err(err) = link_alias(&current_exe, &alias)
    {
        eprintln!(
            "WARNING: proceeding without the embedded code-mode host: could not create {}: {err}",
            alias.display()
        );
        return None;
    }
    InstallContext::set_code_mode_host_program_override(alias.clone());
    Some(alias)
}

/// The arg0 alias directory, identified through helpers that only ever live there.
fn alias_directory(arg0_paths: &Arg0DispatchPaths) -> Option<PathBuf> {
    let helper = arg0_paths.main_execve_wrapper_exe.as_deref()?;
    let helper_dir = helper.parent()?;
    // The sandbox helper falls back to the executable itself when no alias
    // directory could be created; never write next to the real binary.
    if arg0_paths.codex_self_exe.as_deref().and_then(Path::parent) == Some(helper_dir) {
        return None;
    }
    Some(helper_dir.to_path_buf())
}

#[cfg(unix)]
fn link_alias(current_exe: &Path, alias: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(current_exe, alias)
}

#[cfg(not(unix))]
fn link_alias(_current_exe: &Path, alias: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("{} needs a symlink-capable platform", alias.display()),
    ))
}
