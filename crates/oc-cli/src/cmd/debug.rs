use std::path::{Path, PathBuf};
use std::sync::Arc;

use oc_catalog::lsp_config::ResolvedLsp;
use oc_catalog::skill::discovery::SkillOptions;
use oc_config::schema::Config;
use oc_lsp::{Manager, RestartPolicy, ServerRegistry};
use oc_search::{Backend, GlobRequest, GrepRequest, NeverCancelled};
use oc_snapshot::{Location, Store};
use serde::Serialize;

use crate::command::{
    DebugAgentArgs, DebugArgs, DebugCommand, DebugLspCommand, DebugRgCommand, DebugSnapshotCommand,
};
use crate::environment::StartupEnvironment;

pub(super) fn execute(args: &DebugArgs, environment: &StartupEnvironment) -> Result<(), String> {
    let command = args
        .command
        .as_ref()
        .ok_or("debug subcommand is required")?;
    match command {
        DebugCommand::Paths => paths(environment),
        DebugCommand::Config => {
            let context = Context::resolve(environment)?;
            print_json(&context.config)
        }
        DebugCommand::Agent(args) => {
            let context = Context::resolve(environment)?;
            agent(args, &context)
        }
        DebugCommand::Skill => {
            let context = Context::resolve(environment)?;
            skill(&context)
        }
        DebugCommand::Rg(args) => {
            let context = Context::resolve(environment)?;
            rg(
                args.command
                    .as_ref()
                    .ok_or("debug rg subcommand is required")?,
                &context,
            )
        }
        DebugCommand::Lsp(args) => {
            let context = Context::resolve(environment)?;
            lsp(
                args.command
                    .as_ref()
                    .ok_or("debug lsp subcommand is required")?,
                &context,
            )
        }
        DebugCommand::Snapshot(args) => {
            let context = Context::resolve(environment)?;
            snapshot(
                args.command
                    .as_ref()
                    .ok_or("debug snapshot subcommand is required")?,
                &context,
            )
        }
    }
}

struct Context {
    directory: PathBuf,
    worktree: Option<PathBuf>,
    env: oc_paths::Env,
    config: Config,
}

impl Context {
    fn resolve(environment: &StartupEnvironment) -> Result<Self, String> {
        let directory = std::env::current_dir().map_err(to_string)?;
        let project = oc_paths::project::resolve_project(&directory);
        let worktree = project.vcs.as_ref().map(|_| project.directory.clone());
        let env = environment.resolved().clone();
        let config =
            oc_config::discovery::discover_with(&oc_config::discovery::DiscoveryOptions::new(
                &directory,
                worktree.as_deref(),
                env.clone(),
            ))
            .map_err(to_string)?;
        Ok(Self {
            directory,
            worktree,
            env,
            config,
        })
    }
}

fn paths(environment: &StartupEnvironment) -> Result<(), String> {
    let layout = oc_paths::Layout::resolve(environment.resolved());
    print!("{}", layout.debug_paths_dump());
    Ok(())
}

fn agent(args: &DebugAgentArgs, context: &Context) -> Result<(), String> {
    if args.tool.is_some() || args.params.is_some() {
        return Err(
            "debug agent tool execution requires the model/session/permission runtime and is not available through the catalog-only debug path"
                .to_owned(),
        );
    }
    let agents = oc_catalog::agent::load(
        &context.directory,
        context.worktree.as_deref(),
        &context.env,
    )
    .map_err(to_string)?;
    let entry = agents
        .into_iter()
        .find(|entry| entry.name == args.name)
        .ok_or_else(|| {
            format!(
                "Agent {} not found, run 'opencode-rust agent list' to get an agent list",
                args.name
            )
        })?;
    print_json(&entry)
}

fn skill(context: &Context) -> Result<(), String> {
    let options = SkillOptions::from_config(
        &context.directory,
        context.worktree.as_deref(),
        &context.env,
        &context.config,
    );
    let runtime = runtime()?;
    let skills = runtime.block_on(oc_catalog::skill::load(&options));
    let mut output = serde_json::Map::new();
    for entry in skills.all() {
        output.insert(
            entry.name.clone(),
            serde_json::to_value(entry).map_err(to_string)?,
        );
    }
    print_json(&output)
}

fn rg(command: &DebugRgCommand, context: &Context) -> Result<(), String> {
    let backend = Backend::from_env();
    match command {
        DebugRgCommand::Files {
            query: _,
            glob,
            limit,
        } => {
            let request = GlobRequest::new(
                &context.directory,
                glob.as_deref().unwrap_or("**/*"),
                limit.unwrap_or(10_000),
            );
            let result = backend.glob(&request, &NeverCancelled).map_err(to_string)?;
            for entry in result.items {
                println!("{}", entry.path);
            }
            Ok(())
        }
        DebugRgCommand::Search {
            pattern,
            glob,
            limit,
        } => {
            let mut request =
                GrepRequest::new(&context.directory, pattern, limit.unwrap_or(10_000));
            if let Some(include) = combined_glob(glob) {
                request = request.with_include(Some(include));
            }
            let result = backend.grep(&request, &NeverCancelled).map_err(to_string)?;
            print_json(&result.items)
        }
    }
}

fn combined_glob(globs: &[String]) -> Option<String> {
    match globs {
        [] => None,
        [only] => Some(only.clone()),
        many => Some(format!("{{{}}}", many.join(","))),
    }
}

fn snapshot(command: &DebugSnapshotCommand, context: &Context) -> Result<(), String> {
    let store = Store::open(Location::discover(&context.directory));
    match command {
        DebugSnapshotCommand::Track => {
            if let Some(hash) = store.track().map_err(to_string)? {
                println!("{hash}");
            }
            Ok(())
        }
        DebugSnapshotCommand::Patch { hash } => {
            let patch = store.patch(hash).map_err(to_string)?;
            print_json(&patch)
        }
        DebugSnapshotCommand::Diff { hash } => {
            let diff = store.diff(hash).map_err(to_string)?;
            println!("{diff}");
            Ok(())
        }
    }
}

fn lsp(command: &DebugLspCommand, context: &Context) -> Result<(), String> {
    let resolved = ResolvedLsp::resolve(context.config.lsp.as_ref());
    let registry = Arc::new(ServerRegistry::offline(&resolved));
    let manager = Manager::new(&context.directory, registry, RestartPolicy::default());
    let runtime = runtime()?;
    runtime.block_on(async {
        let result: Result<serde_json::Value, String> = match command {
            DebugLspCommand::Diagnostics { file } => {
                let path = resolve_path(&context.directory, file)?;
                manager
                    .diagnostics(&path)
                    .await
                    .map_err(to_string)
                    .and_then(|value| serde_json::to_value(value).map_err(to_string))
            }
            DebugLspCommand::Symbols { query } => manager
                .workspace_symbols(query)
                .await
                .map_err(to_string)
                .and_then(|value| serde_json::to_value(value).map_err(to_string)),
            DebugLspCommand::DocumentSymbols { uri } => {
                let path = resolve_path(&context.directory, uri)?;
                manager
                    .document_symbols(&path)
                    .await
                    .map_err(to_string)
                    .and_then(|value| serde_json::to_value(value).map_err(to_string))
            }
        };
        manager.shutdown().await;
        let value = result.map_err(to_string)?;
        print_json(&value)
    })
}

fn resolve_path(directory: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.starts_with("file://") {
        return reqwest::Url::parse(raw)
            .map_err(to_string)?
            .to_file_path()
            .map_err(|()| format!("invalid file URI: {raw}"));
    }
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

fn print_json(value: &impl Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(to_string)?
    );
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(to_string)
}

fn to_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_search_globs_become_one_brace_expression() {
        assert_eq!(combined_glob(&[]), None);
        assert_eq!(combined_glob(&["*.rs".to_owned()]).as_deref(), Some("*.rs"));
        assert_eq!(
            combined_glob(&["*.rs".to_owned(), "*.toml".to_owned()]).as_deref(),
            Some("{*.rs,*.toml}")
        );
    }

    #[test]
    fn paths_accept_file_uris_and_workspace_relative_values() {
        assert_eq!(
            resolve_path(Path::new("/workspace"), "src/lib.rs").expect("relative path"),
            Path::new("/workspace/src/lib.rs")
        );
        assert_eq!(
            resolve_path(Path::new("/workspace"), "file:///tmp/lib.rs").expect("file URI"),
            Path::new("/tmp/lib.rs")
        );
    }
}
