use std::path::{Path, PathBuf};

use crate::command::{PluginArgs, PluginCommand, PluginInstallArgs, PluginRemoveArgs};
use crate::environment::StartupEnvironment;

pub(super) fn execute(args: &PluginArgs, environment: &StartupEnvironment) -> Result<(), String> {
    match args
        .command
        .as_ref()
        .ok_or("plugin subcommand is required")?
    {
        PluginCommand::List { dir } => list(dir.as_deref(), environment),
        PluginCommand::Add(args) => install(args, environment, zuno_extension::InstallMode::Add),
        PluginCommand::Update(args) => {
            install(args, environment, zuno_extension::InstallMode::Update)
        }
        PluginCommand::Remove(args) => remove(args, environment),
    }
}

fn list(directory: Option<&Path>, environment: &StartupEnvironment) -> Result<(), String> {
    let directory = selected_directory(directory)?;
    let project = zuno_paths::project::resolve_project(&directory);
    let worktree = project.vcs.as_ref().map(|_| project.directory.as_path());
    let scope = zuno_extension::Scope::new(worktree.unwrap_or(directory.as_path()));
    let packages = zuno_extension::discover_static(&directory, worktree, environment.resolved())
        .map_err(to_string)?;
    let resolved = zuno_extension::resolve_active(&scope, &packages, environment.extensions())
        .map_err(to_string)?;
    if resolved.packages().is_empty() {
        println!("No plugins active for {}", directory.display());
        return Ok(());
    }
    for entry in resolved.packages() {
        let source = match &entry.origin {
            zuno_extension::PackageOrigin::Static { manifest } => manifest.display().to_string(),
            zuno_extension::PackageOrigin::Process => "current process".to_owned(),
        };
        let runtime =
            entry
                .package
                .runtime
                .as_ref()
                .map_or("declarative", |runtime| match runtime {
                    zuno_extension::PluginRuntime::Wasi { .. } => "wasi",
                    zuno_extension::PluginRuntime::Process { .. } => "process",
                });
        println!("{} ({runtime})", entry.package.id);
        println!("    {source}");
        if !entry.package.description.is_empty() {
            println!("    {}", entry.package.description);
        }
        print_names("agents", entry.package.agents.keys());
        print_names("workflows", entry.package.workflows.keys());
        print_names(
            "skills",
            entry.package.skills.iter().map(|skill| skill.name.as_str()),
        );
        print_names("tools", entry.package.tools.keys());
        if let Some(runtime) = &entry.package.runtime {
            print_names(
                "capabilities",
                runtime
                    .capabilities()
                    .iter()
                    .map(|capability| capability.as_str()),
            );
            if let zuno_extension::PluginRuntime::Wasi { environment, .. } = runtime {
                print_names("environment", environment.iter().map(String::as_str));
            }
        }
    }
    println!(
        "{} plugin{}",
        resolved.packages().len(),
        if resolved.packages().len() == 1 {
            ""
        } else {
            "s"
        }
    );
    Ok(())
}

fn print_names<'a>(label: &str, values: impl Iterator<Item = &'a str>) {
    let values = values.collect::<Vec<_>>();
    if !values.is_empty() {
        println!("    {label}: {}", values.join(", "));
    }
}

fn install(
    args: &PluginInstallArgs,
    environment: &StartupEnvironment,
    mode: zuno_extension::InstallMode,
) -> Result<(), String> {
    let directory = selected_directory(args.dir.as_deref())?;
    let target = target_config_root(args.project, &directory, environment);
    let source = resolve_source(&directory, &args.source);
    let installed = zuno_extension::install_local(&source, &target, mode).map_err(to_string)?;
    let action = match mode {
        zuno_extension::InstallMode::Add => "Installed",
        zuno_extension::InstallMode::Update => "Updated",
    };
    println!(
        "{action} plugin `{}` at {}",
        installed.id,
        installed.destination.display()
    );
    println!("The package is available to newly assembled Zuno hosts.");
    Ok(())
}

fn remove(args: &PluginRemoveArgs, environment: &StartupEnvironment) -> Result<(), String> {
    let directory = selected_directory(args.dir.as_deref())?;
    let target = target_config_root(args.project, &directory, environment);
    let removed = zuno_extension::remove_installed(&args.id, &target).map_err(to_string)?;
    println!("Removed plugin `{}` from {}", args.id, removed.display());
    println!("Running hosts retain their current composition until they stop.");
    Ok(())
}

fn selected_directory(directory: Option<&Path>) -> Result<PathBuf, String> {
    let directory = match directory {
        Some(directory) if directory.is_absolute() => directory.to_path_buf(),
        Some(directory) => std::env::current_dir().map_err(to_string)?.join(directory),
        None => std::env::current_dir().map_err(to_string)?,
    };
    if directory.is_dir() {
        Ok(directory)
    } else {
        Err(format!("directory does not exist: {}", directory.display()))
    }
}

fn target_config_root(
    project_target: bool,
    directory: &Path,
    environment: &StartupEnvironment,
) -> PathBuf {
    if project_target {
        let project = zuno_paths::project::resolve_project(directory);
        let root = project
            .vcs
            .as_ref()
            .map_or(directory, |_| project.directory.as_path());
        root.join(zuno_paths::PROJECT_DIRECTORY)
    } else {
        zuno_paths::Layout::resolve(environment.resolved())
            .config()
            .to_path_buf()
    }
}

fn resolve_source(directory: &Path, source: &Path) -> PathBuf {
    if source.is_absolute() {
        source.to_path_buf()
    } else {
        directory.join(source)
    }
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}
