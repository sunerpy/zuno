use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use clap_complete::{Generator as _, Shell};

use crate::CompletionArgs;

pub(super) fn execute(args: &CompletionArgs) -> Result<(), String> {
    if args.install {
        return install(args.shell);
    }

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_completion(args.shell, &mut stdout)
        .map_err(|error| format!("failed to write completion script to stdout: {error}"))
}

fn install(shell: Shell) -> Result<(), String> {
    let path = install_path(shell)?;
    let mut script = Vec::new();
    write_completion(shell, &mut script)
        .map_err(|error| format!("failed to generate {shell} completion: {error}"))?;
    atomic_write(&path, &script)?;

    println!("Installed {shell} completion at {}", path.display());
    println!("No shell profile was modified.");
    println!("Activation: {}", activation_hint(shell));
    Ok(())
}

fn write_completion(shell: Shell, writer: &mut dyn io::Write) -> io::Result<()> {
    let mut command = crate::clap_command();
    command.set_bin_name("zuno");
    command.build();
    shell.try_generate(&command, writer)
}

fn install_path(shell: Shell) -> Result<PathBuf, String> {
    match shell {
        Shell::Bash => Ok(data_home()?
            .join("bash-completion")
            .join("completions")
            .join("zuno")),
        Shell::Zsh => Ok(home_dir()?.join(".zsh").join("completions").join("_zuno")),
        Shell::Fish => Ok(config_home()?
            .join("fish")
            .join("completions")
            .join("zuno.fish")),
        Shell::PowerShell => Ok(local_data_home()?
            .join("zuno")
            .join("completions")
            .join("_zuno.ps1")),
        Shell::Elvish => Ok(config_home()?.join("elvish").join("lib").join("zuno.elv")),
        _ => Err(format!(
            "completion installation is not implemented for shell `{shell}`"
        )),
    }
}

fn env_path(name: &OsStr) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> Result<PathBuf, String> {
    env_path(OsStr::new("HOME"))
        .or_else(|| env_path(OsStr::new("USERPROFILE")))
        .ok_or_else(|| {
            "cannot install shell completion because HOME and USERPROFILE are unset".to_owned()
        })
}

fn data_home() -> Result<PathBuf, String> {
    env_path(OsStr::new("XDG_DATA_HOME"))
        .map_or_else(|| Ok(home_dir()?.join(".local").join("share")), Ok)
}

fn config_home() -> Result<PathBuf, String> {
    env_path(OsStr::new("XDG_CONFIG_HOME")).map_or_else(|| Ok(home_dir()?.join(".config")), Ok)
}

fn local_data_home() -> Result<PathBuf, String> {
    match env_path(OsStr::new("LOCALAPPDATA")) {
        Some(path) => Ok(path),
        None => data_home(),
    }
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("completion path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create completion directory {}: {error}",
            parent.display()
        )
    })?;

    let mut temporary = tempfile::Builder::new()
        .prefix(".zuno-completion-")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "failed to create a temporary completion file in {}: {error}",
                parent.display()
            )
        })?;
    temporary
        .as_file_mut()
        .write_all(contents)
        .map_err(|error| {
            format!(
                "failed to write temporary completion file in {}: {error}",
                parent.display()
            )
        })?;
    temporary.as_file_mut().sync_all().map_err(|error| {
        format!(
            "failed to synchronize temporary completion file in {}: {error}",
            parent.display()
        )
    })?;
    temporary.persist(path).map_err(|error| {
        format!(
            "failed to atomically install completion at {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

fn activation_hint(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => "start a new bash session, or source the installed file shown above.",
        Shell::Zsh => {
            "add the installed file's parent directory to fpath, then run `autoload -Uz compinit && compinit`."
        }
        Shell::Fish => "start a new fish session; fish loads the installed file automatically.",
        Shell::PowerShell => {
            "dot-source the installed file shown above in the current PowerShell session."
        }
        Shell::Elvish => "run `use zuno` in the current Elvish session.",
        _ => "load the installed script in the current shell session.",
    }
}
