use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use zuno_catalog::lsp_config::ResolvedLsp;
use zuno_catalog::skill::discovery::SkillOptions;
use zuno_config::schema::Config;
use zuno_config::schema::plugin::PluginSpec;
use zuno_lsp::{Manager, RestartPolicy, ServerRegistry};
use zuno_plugin::{ConfigDirectory, ConfigLayer, PluginScope, discover_plugins};
use zuno_search::{Backend, GlobRequest, GrepRequest, NeverCancelled};
use zuno_snapshot::{Location, Store};

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
            config(&context)
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

#[derive(Serialize)]
struct PluginOriginOutput {
    spec: PluginSpec,
    source: String,
    scope: &'static str,
}

fn config(context: &Context) -> Result<(), String> {
    let agents = zuno_catalog::agent::load_map(
        &context.directory,
        context.worktree.as_deref(),
        &context.env,
    )
    .map_err(to_string)?;
    let commands = zuno_catalog::command::load_map(
        &context.directory,
        context.worktree.as_deref(),
        &context.env,
    )
    .map_err(to_string)?;
    let origins = plugin_origins(context)?;

    let mut output = serde_json::to_value(&context.config).map_err(to_string)?;
    let object = output
        .as_object_mut()
        .ok_or("resolved config did not serialize as an object")?;
    object.insert(
        "agent".to_owned(),
        serde_json::to_value(agents.agents).map_err(to_string)?,
    );
    object.insert(
        "command".to_owned(),
        serde_json::to_value(commands).map_err(to_string)?,
    );
    if !origins.is_empty() {
        object.insert(
            "plugin".to_owned(),
            serde_json::to_value(
                origins
                    .iter()
                    .map(|origin| &origin.spec)
                    .collect::<Vec<_>>(),
            )
            .map_err(to_string)?,
        );
        object.insert(
            "plugin_origins".to_owned(),
            serde_json::to_value(origins).map_err(to_string)?,
        );
    }
    // Reported unconditionally, and alongside the discovered plugins rather than
    // instead of them: "these were found but the host is off" is the state a user
    // whose plugins stopped running actually needs to see.
    let policy = super::plugin_runtime::JsPluginPolicy::resolve(&context.config, &context.env);
    object.insert(
        "plugin_runtime_resolved".to_owned(),
        serde_json::json!({
            "javascript": policy.enabled,
            "source": policy.source,
        }),
    );
    normalize_json_numbers(&mut output);
    print_json(&output)
}

fn normalize_json_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_json_numbers(value);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                normalize_json_numbers(value);
            }
        }
        serde_json::Value::Number(number) if number.is_f64() => {
            let Some(value) = number.as_f64() else {
                return;
            };
            const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
            if value.fract() != 0.0 || value.abs() > MAX_SAFE_INTEGER {
                return;
            }
            if value >= 0.0 {
                *number = serde_json::Number::from(value as u64);
            } else {
                *number = serde_json::Number::from(value as i64);
            }
        }
        _ => {}
    }
}

fn plugin_origins(context: &Context) -> Result<Vec<PluginOriginOutput>, String> {
    let layout = zuno_paths::Layout::resolve(&context.env);
    let mut origins = Vec::new();

    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        add_plugin_file(
            &mut origins,
            &layout.config().join(name),
            layout.config().display().to_string(),
            PluginScope::Global,
        )?;
    }
    if let Some(value) = context.env.truthy_value("OPENCODE_CONFIG") {
        let path = resolve_path(&context.directory, value)?;
        let scope = plugin_scope(context, &path);
        add_plugin_file(&mut origins, &path, path.display().to_string(), scope)?;
    }
    if !layout.project_config_disabled() {
        for path in zuno_paths::Layout::config_files(
            "opencode",
            &context.directory,
            context.worktree.as_deref(),
        ) {
            add_plugin_file(
                &mut origins,
                &path,
                path.display().to_string(),
                PluginScope::Local,
            )?;
        }
    }

    let directories = layout.config_directories(&context.directory, context.worktree.as_deref());
    for directory in &directories {
        let is_override = layout
            .config_dir_override()
            .filter(|value| !value.is_empty())
            .is_some_and(|value| directory == Path::new(value));
        if directory.to_string_lossy().ends_with(".zuno") || is_override {
            for path in zuno_paths::Layout::file_in_directory(directory, "opencode") {
                let scope = plugin_scope(context, &path);
                add_plugin_file(&mut origins, &path, path.display().to_string(), scope)?;
            }
        }

        let scope = plugin_scope(context, directory);
        let discovered =
            discover_plugins(&[], &[ConfigDirectory::new(directory, scope)]).map_err(to_string)?;
        origins.extend(discovered.into_iter().map(|plugin| PluginOriginOutput {
            spec: plugin.spec,
            source: directory.display().to_string(),
            scope: scope_label(scope),
        }));
    }

    if let Some(text) = context.env.truthy_value("OPENCODE_CONFIG_CONTENT") {
        let layer = Config::from_json_str(Path::new("OPENCODE_CONFIG_CONTENT"), text)
            .map_err(|error| error.report())?;
        let declaration = context.directory.join("OPENCODE_CONFIG_CONTENT");
        add_plugin_config(
            &mut origins,
            &layer,
            &declaration,
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            PluginScope::Local,
        )?;
    }

    let managed = managed_config_dir(&context.env);
    if managed.exists() {
        for path in zuno_paths::Layout::file_in_directory(&managed, "opencode") {
            add_plugin_file(
                &mut origins,
                &path,
                path.display().to_string(),
                PluginScope::Global,
            )?;
        }
    }

    Ok(deduplicate_plugin_origins(origins))
}

fn add_plugin_file(
    origins: &mut Vec<PluginOriginOutput>,
    path: &Path,
    source: String,
    scope: PluginScope,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path).map_err(to_string)?;
    let strict = zuno_config::discovery::strip_jsonc(&text);
    let config = Config::from_json_str(path, &strict).map_err(|error| error.report())?;
    add_plugin_config(origins, &config, path, source, scope)
}

fn add_plugin_config(
    origins: &mut Vec<PluginOriginOutput>,
    config: &Config,
    declaration_source: &Path,
    source: String,
    scope: PluginScope,
) -> Result<(), String> {
    let discovered = discover_plugins(&[ConfigLayer::new(declaration_source, scope, config)], &[])
        .map_err(to_string)?;
    origins.extend(discovered.into_iter().map(|plugin| PluginOriginOutput {
        spec: plugin.spec,
        source: source.clone(),
        scope: scope_label(scope),
    }));
    Ok(())
}

fn managed_config_dir(env: &zuno_paths::Env) -> PathBuf {
    if let Some(path) = env.truthy_value("OPENCODE_TEST_MANAGED_CONFIG_DIR") {
        return PathBuf::from(path);
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/zuno")
    }
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(env.truthy_value("ProgramData").unwrap_or("C:\\ProgramData")).join("zuno")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        PathBuf::from("/etc/zuno")
    }
}

fn plugin_scope(context: &Context, source: &Path) -> PluginScope {
    let local_root = context.worktree.as_deref().unwrap_or(&context.directory);
    if source.starts_with(local_root) {
        PluginScope::Local
    } else {
        PluginScope::Global
    }
}

const fn scope_label(scope: PluginScope) -> &'static str {
    match scope {
        PluginScope::Global => "global",
        PluginScope::Local => "local",
    }
}

fn plugin_identity(spec: &str) -> &str {
    if spec.starts_with("file://") {
        return spec;
    }
    let search_from = usize::from(spec.starts_with('@'));
    spec[search_from..]
        .rfind('@')
        .map_or(spec, |index| &spec[..search_from + index])
}

fn deduplicate_plugin_origins(origins: Vec<PluginOriginOutput>) -> Vec<PluginOriginOutput> {
    let mut seen = HashSet::new();
    let mut reversed = Vec::new();
    for origin in origins.into_iter().rev() {
        if seen.insert(plugin_identity(origin.spec.name()).to_owned()) {
            reversed.push(origin);
        }
    }
    reversed.reverse();
    reversed
}

struct Context {
    directory: PathBuf,
    worktree: Option<PathBuf>,
    env: zuno_paths::Env,
    config: Config,
}

impl Context {
    fn resolve(environment: &StartupEnvironment) -> Result<Self, String> {
        let directory = std::env::current_dir().map_err(to_string)?;
        let project = zuno_paths::project::resolve_project(&directory);
        let worktree = project.vcs.as_ref().map(|_| project.directory.clone());
        let env = environment.resolved().clone();
        let config =
            zuno_config::discovery::discover_with(&zuno_config::discovery::DiscoveryOptions::new(
                &directory,
                worktree.as_deref(),
                env.clone(),
            ))
            .map_err(|error| error.report())?;
        Ok(Self {
            directory,
            worktree,
            env,
            config,
        })
    }
}

fn paths(environment: &StartupEnvironment) -> Result<(), String> {
    let layout = zuno_paths::Layout::resolve(environment.resolved());
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
    let agents = zuno_catalog::agent::load(
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
                "Agent {} not found, run 'zuno agent list' to get an agent list",
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
    let skills = runtime.block_on(zuno_catalog::skill::load(&options));
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
