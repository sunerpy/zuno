use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::OptionalExtension;
use serde::Serialize;
use zuno_catalog::lsp_config::ResolvedLsp;
use zuno_catalog::skill::discovery::SkillOptions;
use zuno_config::schema::Config;
use zuno_lsp::{Manager, RestartPolicy, ServerRegistry};
use zuno_search::{GlobRequest, GrepRequest, NeverCancelled, Ripgrep};
use zuno_snapshot::{Location, Store};

use crate::command::{
    DebugAgentArgs, DebugArgs, DebugCommand, DebugLspCommand, DebugPromptArgs, DebugRgCommand,
    DebugSnapshotCommand,
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
        DebugCommand::Prompt(args) => prompt(args),
        DebugCommand::Permissions => {
            let context = Context::resolve(environment)?;
            permissions(&context)
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

fn permissions(context: &Context) -> Result<(), String> {
    let rules = context
        .config
        .permission
        .as_ref()
        .map(|permission| permission.rules.clone())
        .unwrap_or_default();
    print_json(&serde_json::json!({
        "configuredMode": context.config.permission_mode(),
        "mode": context.config.effective_permission_mode(),
        "rules": rules,
        "strictSideEffectsRequireApproval": context.config.strict_authorization(),
        "allowAllStillEnforces": [
            "explicit deny",
            "catastrophic shell denial",
            "sandbox authority",
            "argument validation"
        ],
    }))
}

fn prompt(args: &DebugPromptArgs) -> Result<(), String> {
    let pool = zuno_db::Pool::open_default().map_err(to_string)?;
    let mut connection = pool.get().map_err(to_string)?;
    zuno_db::migration::apply(&mut connection).map_err(to_string)?;
    let row = match (&args.session_id, args.turn) {
        (Some(session_id), Some(turn)) => connection
            .query_row(
                "SELECT id, aggregate_id, seq, data FROM event \
                 WHERE type = 'session.prompt.assembled.1' AND aggregate_id = ?1 \
                 AND CAST(json_extract(data, '$.step') AS INTEGER) = ?2 \
                 ORDER BY seq DESC LIMIT 1",
                rusqlite::params![session_id, turn],
                prompt_row,
            )
            .optional(),
        (Some(session_id), None) => connection
            .query_row(
                "SELECT id, aggregate_id, seq, data FROM event \
                 WHERE type = 'session.prompt.assembled.1' AND aggregate_id = ?1 \
                 ORDER BY seq DESC LIMIT 1",
                [session_id],
                prompt_row,
            )
            .optional(),
        (None, None) => connection
            .query_row(
                "SELECT id, aggregate_id, seq, data FROM event \
                 WHERE type = 'session.prompt.assembled.1' ORDER BY rowid DESC LIMIT 1",
                [],
                prompt_row,
            )
            .optional(),
        (None, Some(_)) => unreachable!("turn is positional after session"),
    }
    .map_err(to_string)?;
    let Some((event_id, session_id, sequence, data)) = row else {
        let target = args
            .session_id
            .as_deref()
            .map_or_else(|| "the database".to_owned(), |id| format!("session `{id}`"));
        return Err(format!("no prompt receipt found for {target}"));
    };
    let mut properties: serde_json::Value = serde_json::from_str(&data).map_err(to_string)?;
    if !args.show_sensitive {
        redact_prompt_content(&mut properties);
    }
    print_json(&serde_json::json!({
        "eventId": event_id,
        "eventType": "session.prompt.assembled",
        "sessionId": session_id,
        "sequence": sequence,
        "properties": properties,
    }))
}

fn prompt_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, i64, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn redact_prompt_content(value: &mut serde_json::Value) {
    let Some(properties) = value.as_object_mut() else {
        return;
    };
    if let Some(sections) = properties
        .get_mut("sections")
        .and_then(serde_json::Value::as_array_mut)
    {
        for section in sections {
            if let Some(section) = section.as_object_mut() {
                section.insert("content".to_owned(), serde_json::json!("<redacted>"));
            }
        }
    }
    if properties.contains_key("actualSystemPrompt") {
        properties.insert(
            "actualSystemPrompt".to_owned(),
            serde_json::json!("<redacted>"),
        );
    }
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
    let ripgrep = Ripgrep::discover().map_err(to_string)?;
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
            let result = ripgrep.glob(&request, &NeverCancelled).map_err(to_string)?;
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
            let result = ripgrep.grep(&request, &NeverCancelled).map_err(to_string)?;
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
    let manager = Manager::new(
        &context.directory,
        registry,
        RestartPolicy::default(),
        std::num::NonZeroUsize::new(usize::from(
            context.config.resolved_concurrency().lsp_requests,
        ))
        .expect("configuration validates LSP concurrency"),
    );
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
    fn prompt_debug_redacts_model_visible_bodies_but_keeps_provenance() {
        let mut receipt = serde_json::json!({
            "assemblySha256": "assembly-digest",
            "actualSha256": "actual-digest",
            "actualSystemPrompt": "hook-transformed secret",
            "sections": [{
                "id": "instructions.project.0",
                "role": "project_instructions",
                "source": "/repo/AGENTS.md",
                "sha256": "section-digest",
                "bytes": 12,
                "content": "private rule"
            }]
        });

        redact_prompt_content(&mut receipt);

        assert_eq!(receipt["sections"][0]["content"], "<redacted>");
        assert_eq!(receipt["actualSystemPrompt"], "<redacted>");
        assert_eq!(receipt["sections"][0]["source"], "/repo/AGENTS.md");
        assert_eq!(receipt["sections"][0]["sha256"], "section-digest");
        assert_eq!(receipt["assemblySha256"], "assembly-digest");
    }

    #[test]
    fn prompt_debug_does_not_invent_a_post_hook_body_when_none_was_stored() {
        let mut receipt = serde_json::json!({
            "sections": [{"content": "private rule"}],
            "hookTransformed": false
        });

        redact_prompt_content(&mut receipt);

        assert_eq!(receipt["sections"][0]["content"], "<redacted>");
        assert!(receipt.get("actualSystemPrompt").is_none());
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
